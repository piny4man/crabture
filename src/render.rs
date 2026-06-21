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

use crate::session::SessionCommand;
use crate::session::{CaptureMode, GraphicalPreferences, OutputDestination, SaveLocationChoice};
use ab_glyph::{Font, FontRef, ScaleFont, point};
use std::sync::OnceLock;
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, PixmapPaint, Transform};

// ---------------------------------------------------------------------------
// Theme (logical units; multiplied by `scale` at render time)
// ---------------------------------------------------------------------------

/// Translucent dark panel material.
const PANEL_FILL: (u8, u8, u8, u8) = (32, 32, 36, 236);
/// Hairline panel border.
const PANEL_BORDER: (u8, u8, u8, u8) = (255, 255, 255, 30);
/// Thin separator between toolbar groups.
const SEPARATOR: (u8, u8, u8, u8) = (255, 255, 255, 28);
/// macOS-blue accent for the active mode and the Capture action.
const ACCENT: (u8, u8, u8, u8) = (10, 132, 255, 255);
/// Primary label colour.
const TEXT: (u8, u8, u8, u8) = (255, 255, 255, 236);
/// Secondary / muted label colour (categories, shortcut hints, caret).
const TEXT_MUTED: (u8, u8, u8, u8) = (235, 235, 245, 150);
/// Hover highlight behind a button.
const HOVER: (u8, u8, u8, u8) = (255, 255, 255, 22);

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
const VAL_CARET_GAP: f32 = 6.0;
const CARET_W: f32 = 7.0;
const CARET_H: f32 = 4.0;
const SHORT_GAP: f32 = 8.0;
const HINT_GAP: f32 = 7.0;
const BTN_RADIUS: f32 = 9.0;
const PANEL_RADIUS: f32 = 13.0;

const PANEL_H: f32 = BTN_H + 2.0 * PANEL_PAD_Y;

const FONT_BYTES: &[u8] = include_bytes!("../assets/Roboto-Medium.ttf");
const AREA_SVG: &[u8] = include_bytes!("../assets/area.svg");
const WINDOW_SVG: &[u8] = include_bytes!("../assets/window.svg");
const FULLSCREEN_SVG: &[u8] = include_bytes!("../assets/fullscreen.svg");

fn font() -> &'static FontRef<'static> {
    static FONT: OnceLock<FontRef<'static>> = OnceLock::new();
    FONT.get_or_init(|| FontRef::try_from_slice(FONT_BYTES).expect("bundled Roboto font is valid"))
}

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
        hint: &'static str,
        accent: bool,
    },
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
fn text_width(text: &str, px: f32) -> f32 {
    let scaled = font().as_scaled(px);
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
        SaveLocationChoice::CurrentDirectory => "Current dir",
    }
}

fn mode_width(label: &str, shortcut: char) -> f32 {
    BTN_PAD_X
        + ICON
        + ICON_GAP
        + text_width(label, LABEL_PX)
        + SHORT_GAP
        + text_width(&shortcut.to_string(), SHORT_PX)
        + BTN_PAD_X
}

fn chip_width(category: &str, value: &str, shortcut: char) -> f32 {
    BTN_PAD_X
        + text_width(category, CAT_PX)
        + CAT_VAL_GAP
        + text_width(value, VAL_PX)
        + VAL_CARET_GAP
        + CARET_W
        + SHORT_GAP
        + text_width(&shortcut.to_string(), SHORT_PX)
        + BTN_PAD_X
}

fn action_width(label: &str, hint: &str) -> f32 {
    BTN_PAD_X + text_width(label, LABEL_PX) + HINT_GAP + text_width(hint, SHORT_PX) + BTN_PAD_X
}

/// Build the toolbar layout in logical surface coordinates.
pub fn toolbar_layout(
    surface_w: usize,
    surface_h: usize,
    prefs: GraphicalPreferences,
) -> Vec<ToolButton> {
    if surface_w == 0 || surface_h == 0 {
        return Vec::new();
    }

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
            let w = mode_width("Area", 'A');
            (k, w, 0)
        },
        {
            let k = ToolKind::Mode {
                icon: Icon::Window,
                label: "Window",
                shortcut: 'W',
                mode: CaptureMode::Window,
            };
            let w = mode_width("Window", 'W');
            (k, w, 0)
        },
        {
            let k = ToolKind::Mode {
                icon: Icon::FullScreen,
                label: "Full",
                shortcut: 'F',
                mode: CaptureMode::FullScreen,
            };
            let w = mode_width("Full", 'F');
            (k, w, 0)
        },
        {
            let w = chip_width("Output", &output_val, 'O');
            let k = ToolKind::Chip {
                category: "Output",
                value: output_val,
                shortcut: 'O',
            };
            (k, w, 1)
        },
        {
            let w = chip_width("Format", &format_val, 'P');
            let k = ToolKind::Chip {
                category: "Format",
                value: format_val,
                shortcut: 'P',
            };
            (k, w, 1)
        },
        {
            let w = chip_width("Location", &location_val, 'L');
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
                hint: "\u{23ce}",
                accent: true,
            };
            let w = action_width("Capture", "\u{23ce}");
            (k, w, 2)
        },
        {
            let k = ToolKind::Action {
                label: "Cancel",
                hint: "esc",
                accent: false,
            };
            let w = action_width("Cancel", "esc");
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

/// Draw a small downward caret (▾) with its top-left at `(x, y)`.
fn fill_caret(pm: &mut Pixmap, x: f32, y: f32, c: (u8, u8, u8, u8)) {
    let mut pb = PathBuilder::new();
    pb.move_to(x, y);
    pb.line_to(x + CARET_W, y);
    pb.line_to(x + CARET_W / 2.0, y + CARET_H);
    pb.close();
    if let Some(path) = pb.finish() {
        pm.fill_path(
            &path,
            &solid_paint(c),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
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
fn draw_text(pm: &mut Pixmap, x: f32, baseline: f32, text: &str, px: f32, c: (u8, u8, u8, u8)) {
    let f = font();
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

/// Rasterise a mode icon to a square white pixmap of `px` physical pixels.
fn rasterize_icon(icon: Icon, px: u32) -> Option<Pixmap> {
    if px == 0 {
        return None;
    }
    let svg = String::from_utf8_lossy(icon_bytes(icon)).replace("currentColor", "#ffffff");
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
pub fn render_toolbar(
    layout: &[ToolButton],
    prefs: GraphicalPreferences,
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

    // Panel background + hairline border.
    fill_round_rect(
        &mut pm,
        lx(panel_x),
        ly(panel_y),
        panel_w * scale,
        panel_h * scale,
        PANEL_RADIUS * scale,
        PANEL_FILL,
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
            &solid_paint(PANEL_BORDER),
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
            fill_rect(&mut pm, sep_x, top, scale.max(1.0), bot - top, SEPARATOR);
        }
    }

    for (i, button) in layout.iter().enumerate() {
        let (bx, by, bw, bh) = button.rect;
        let hovered = hovered == Some(i);
        render_button(
            &mut pm,
            button,
            prefs,
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
fn render_button(
    pm: &mut Pixmap,
    button: &ToolButton,
    prefs: GraphicalPreferences,
    scale: f32,
    hovered: bool,
    px: f32,
    py: f32,
    pw: f32,
    ph: f32,
) {
    // Background: accent pill for the active mode / accent action, hover wash
    // otherwise.
    let active_mode = matches!(button.kind, ToolKind::Mode { mode, .. } if mode == prefs.mode);
    let accent_action = matches!(button.kind, ToolKind::Action { accent: true, .. });
    if active_mode || accent_action {
        fill_round_rect(pm, px, py, pw, ph, BTN_RADIUS * scale, ACCENT);
    } else if hovered {
        fill_round_rect(pm, px, py, pw, ph, BTN_RADIUS * scale, HOVER);
    }

    // Baseline: vertically centre text within the button.
    let f = font();
    let label_scaled = f.as_scaled(LABEL_PX * scale);
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
            if let Some(icon_pm) = rasterize_icon(*icon, icon_px) {
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
            draw_text(pm, label_x, baseline, label, LABEL_PX * scale, TEXT);
            let label_w = text_width(label, LABEL_PX) * scale;
            let short_x = label_x + label_w + SHORT_GAP * scale;
            let short_color = if active_mode { TEXT } else { TEXT_MUTED };
            draw_text(
                pm,
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
            draw_text(pm, cat_x, baseline, category, CAT_PX * scale, TEXT_MUTED);
            let cat_w = text_width(category, CAT_PX) * scale;
            let val_x = cat_x + cat_w + CAT_VAL_GAP * scale;
            draw_text(pm, val_x, baseline, value, VAL_PX * scale, TEXT);
            let val_w = text_width(value, VAL_PX) * scale;
            let caret_x = val_x + val_w + VAL_CARET_GAP * scale;
            let caret_y = py + ph / 2.0 - (CARET_H * scale) / 2.0 + scale;
            fill_caret(pm, caret_x, caret_y, TEXT_MUTED);
            let short_x = caret_x + CARET_W * scale + SHORT_GAP * scale;
            draw_text(
                pm,
                short_x,
                baseline,
                &shortcut.to_string(),
                SHORT_PX * scale,
                TEXT_MUTED,
            );
        }
        ToolKind::Action {
            label,
            hint,
            accent,
        } => {
            let label_x = px + BTN_PAD_X * scale;
            let label_color = if *accent { (255, 255, 255, 255) } else { TEXT };
            draw_text(pm, label_x, baseline, label, LABEL_PX * scale, label_color);
            let label_w = text_width(label, LABEL_PX) * scale;
            let hint_x = label_x + label_w + HINT_GAP * scale;
            let hint_color = if *accent {
                (255, 255, 255, 200)
            } else {
                TEXT_MUTED
            };
            draw_text(pm, hint_x, baseline, hint, SHORT_PX * scale, hint_color);
        }
    }
}

/// Source-over composite a premultiplied-RGBA pixmap onto a premultiplied
/// ARGB8888-LE (BGRA byte order) canvas stored as packed `u32`s.
pub fn blit_argb(dst: &mut [u32], dst_w: usize, dst_h: usize, src: &Pixmap, ox: i32, oy: i32) {
    let sw = src.width() as i32;
    let sh = src.height() as i32;
    let data = src.data();
    for sy in 0..sh {
        let dy = oy + sy;
        if dy < 0 || dy >= dst_h as i32 {
            continue;
        }
        for sx in 0..sw {
            let dx = ox + sx;
            if dx < 0 || dx >= dst_w as i32 {
                continue;
            }
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
        blit_argb(&mut dst, 1, 1, &pm, 0, 0);
        assert_eq!(dst[0], pack_argb(255, 255, 255, 255));
    }

    #[test]
    fn transparent_source_leaves_destination() {
        let pm = Pixmap::new(1, 1).unwrap(); // transparent by default
        let mut dst = vec![pack_argb(255, 10, 20, 30); 1];
        blit_argb(&mut dst, 1, 1, &pm, 0, 0);
        assert_eq!(dst[0], pack_argb(255, 10, 20, 30));
    }

    #[test]
    fn blit_clips_to_destination_bounds() {
        let mut pm = Pixmap::new(2, 2).unwrap();
        pm.fill(Color::from_rgba8(255, 255, 255, 255));
        let mut dst = vec![0u32; 1];
        // Offsetting fully off-canvas must not panic and must not write.
        blit_argb(&mut dst, 1, 1, &pm, 5, 5);
        assert_eq!(dst[0], 0);
    }

    #[test]
    fn layout_emits_expected_commands_in_order() {
        let layout = toolbar_layout(1920, 1080, prefs());
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
    fn layout_is_empty_for_zero_sized_surface() {
        assert!(toolbar_layout(0, 0, prefs()).is_empty());
    }

    #[test]
    fn buttons_are_left_to_right_and_non_overlapping() {
        let layout = toolbar_layout(1920, 1080, prefs());
        for pair in layout.windows(2) {
            let a_right = pair[0].rect.0 + pair[0].rect.2;
            assert!(pair[1].rect.0 >= a_right, "buttons must not overlap");
        }
    }

    #[test]
    fn button_at_hits_each_button_center() {
        let layout = toolbar_layout(1920, 1080, prefs());
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
                        let layout = toolbar_layout(1920, 1080, p);
                        let (pm, ox, oy) = render_toolbar(&layout, p, 2, None).unwrap();
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
        let layout = toolbar_layout(1920, 1080, prefs());
        let (p1, _, _) = render_toolbar(&layout, prefs(), 1, None).unwrap();
        let (p2, _, _) = render_toolbar(&layout, prefs(), 2, None).unwrap();
        // Doubling the scale roughly doubles each dimension.
        assert!(p2.width() >= p1.width() * 2 - 2);
        assert!(p2.height() >= p1.height() * 2 - 2);
    }
}
