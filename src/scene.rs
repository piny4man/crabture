//! What one overlay frame shows, and how to repaint only what changed.
//!
//! A [`Scene`] describes a frame in physical pixels: a background fill, the
//! area selection or window highlight outline, the window-mode crosshair, and
//! the toolbar sprite.  [`damage`] lists the rectangles where two scenes can
//! differ and [`paint`] repaints a scene inside one such rectangle, so a
//! buffer showing an older scene is brought up to date without refilling the
//! whole (up to 2880×1800) canvas on every pointer motion.

use crate::render;
use tiny_skia::Pixmap;

/// Half-open physical pixel rectangle `x0..x1 × y0..y1`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rect {
    pub x0: usize,
    pub y0: usize,
    pub x1: usize,
    pub y1: usize,
}

impl Rect {
    pub const fn new(x0: usize, y0: usize, x1: usize, y1: usize) -> Self {
        Self { x0, y0, x1, y1 }
    }

    pub fn is_empty(&self) -> bool {
        self.x1 <= self.x0 || self.y1 <= self.y0
    }

    pub fn area(&self) -> usize {
        if self.is_empty() {
            0
        } else {
            (self.x1 - self.x0) * (self.y1 - self.y0)
        }
    }

    pub fn intersect(&self, other: &Rect) -> Rect {
        Rect {
            x0: self.x0.max(other.x0),
            y0: self.y0.max(other.y0),
            x1: self.x1.min(other.x1),
            y1: self.y1.min(other.y1),
        }
    }

    /// `self` minus `other`, as up to four disjoint rectangles.
    fn minus(&self, other: &Rect) -> Vec<Rect> {
        let i = self.intersect(other);
        if i.is_empty() {
            return vec![*self];
        }
        [
            Rect::new(self.x0, self.y0, self.x1, i.y0),
            Rect::new(self.x0, i.y1, self.x1, self.y1),
            Rect::new(self.x0, i.y0, i.x0, i.y1),
            Rect::new(i.x1, i.y0, self.x1, i.y1),
        ]
        .into_iter()
        .filter(|r| !r.is_empty())
        .collect()
    }
}

/// A filled rectangle with a solid border `thickness` pixels wide: the area
/// selection (cleared interior, 1 px border) or the window highlight.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Outline {
    pub rect: Rect,
    pub fill: u32,
    pub border: u32,
    pub thickness: usize,
}

impl Outline {
    /// The border as four (possibly overlapping) edge rectangles.
    fn edges(&self) -> [Rect; 4] {
        let Rect { x0, y0, x1, y1 } = self.rect;
        let t = self.thickness;
        [
            Rect::new(x0, y0, x1, (y0 + t).min(y1)),
            Rect::new(x0, y1.saturating_sub(t).max(y0), x1, y1),
            Rect::new(x0, y0, (x0 + t).min(x1), y1),
            Rect::new(x1.saturating_sub(t).max(x0), y0, x1, y1),
        ]
    }
}

/// The window-mode crosshair: one horizontal and one vertical line through
/// `(x, y)`, `radius` pixels each way and `thickness` pixels wide.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Crosshair {
    pub x: usize,
    pub y: usize,
    pub radius: usize,
    pub thickness: usize,
    pub color: u32,
}

impl Crosshair {
    fn lines(&self, width: usize, height: usize) -> [Rect; 2] {
        let t = self.thickness.max(1);
        let reach = self.radius.max(t - 1) + 1;
        let horizontal = if self.y < height {
            Rect::new(
                self.x.saturating_sub(self.radius),
                self.y,
                (self.x + reach).min(width),
                (self.y + t).min(height),
            )
        } else {
            Rect::new(0, 0, 0, 0)
        };
        let vertical = if self.x < width {
            Rect::new(
                self.x,
                self.y.saturating_sub(self.radius),
                (self.x + t).min(width),
                (self.y + reach).min(height),
            )
        } else {
            Rect::new(0, 0, 0, 0)
        };
        [horizontal, vertical]
    }

    fn bounds(&self, width: usize, height: usize) -> Rect {
        let screen = Rect::new(0, 0, width, height);
        let reach = self.radius.max(self.thickness.max(1) - 1) + 1;
        Rect::new(
            self.x.saturating_sub(self.radius),
            self.y.saturating_sub(self.radius),
            self.x + reach,
            self.y + reach,
        )
        .intersect(&screen)
    }
}

/// The toolbar pixmap composited at `(ox, oy)`.  `generation` changes
/// whenever the pixmap is re-rendered, so two sprites compare equal only when
/// they draw the same pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Sprite {
    pub ox: i32,
    pub oy: i32,
    pub width: usize,
    pub height: usize,
    pub generation: u64,
}

impl Sprite {
    fn bounds(&self, width: usize, height: usize) -> Rect {
        let clamp = |v: i64, max: usize| v.clamp(0, max as i64) as usize;
        Rect::new(
            clamp(self.ox as i64, width),
            clamp(self.oy as i64, height),
            clamp(self.ox as i64 + self.width as i64, width),
            clamp(self.oy as i64 + self.height as i64, height),
        )
    }
}

/// Everything one overlay frame draws, in physical pixels, in paint order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Scene {
    pub width: usize,
    pub height: usize,
    pub background: u32,
    pub selection: Option<Outline>,
    pub highlight: Option<Outline>,
    pub target: Option<Crosshair>,
    pub toolbar: Option<Sprite>,
}

impl Scene {
    pub fn screen(&self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }
}

/// Above this many rectangles, or this share of the screen, one full repaint
/// is cheaper than many partial ones.
const MAX_DAMAGE_RECTS: usize = 64;
const MAX_DAMAGE_SHARE: f64 = 0.5;

/// The rectangles where a buffer showing `old` (or nothing) must be repainted
/// to show `new`.  Empty when the two frames look the same; the whole screen
/// when there is no previous frame or the background changed.
#[hotpath::measure]
pub fn damage(old: Option<&Scene>, new: &Scene) -> Vec<Rect> {
    let screen = new.screen();
    let Some(old) = old else {
        return vec![screen];
    };
    if old.width != new.width || old.height != new.height || old.background != new.background {
        return vec![screen];
    }
    let (w, h) = (new.width, new.height);
    let mut rects = Vec::new();
    outline_damage(old.selection, new.selection, &mut rects);
    outline_damage(old.highlight, new.highlight, &mut rects);
    if old.target != new.target {
        rects.extend(old.target.map(|t| t.bounds(w, h)));
        rects.extend(new.target.map(|t| t.bounds(w, h)));
    }
    if old.toolbar != new.toolbar {
        rects.extend(old.toolbar.map(|s| s.bounds(w, h)));
        rects.extend(new.toolbar.map(|s| s.bounds(w, h)));
    }

    let mut unique: Vec<Rect> = Vec::with_capacity(rects.len());
    for rect in rects.into_iter().map(|r| r.intersect(&screen)) {
        if !rect.is_empty() && !unique.contains(&rect) {
            unique.push(rect);
        }
    }
    let area: usize = unique.iter().map(Rect::area).sum();
    if unique.len() > MAX_DAMAGE_RECTS || area as f64 > screen.area() as f64 * MAX_DAMAGE_SHARE {
        return vec![screen];
    }
    unique
}

/// Where an outline can look different between two frames.  For the same
/// style that is the symmetric difference of the two rectangles plus both
/// borders: inside both interiors the fill is unchanged, so moving or
/// resizing a large selection repaints thin strips, not its whole area.
fn outline_damage(old: Option<Outline>, new: Option<Outline>, rects: &mut Vec<Rect>) {
    match (old, new) {
        (Some(a), Some(b)) if a == b => {}
        (Some(a), Some(b))
            if a.fill == b.fill && a.border == b.border && a.thickness == b.thickness =>
        {
            rects.extend(a.rect.minus(&b.rect));
            rects.extend(b.rect.minus(&a.rect));
            rects.extend(a.edges());
            rects.extend(b.edges());
        }
        (a, b) => {
            rects.extend(a.map(|o| o.rect));
            rects.extend(b.map(|o| o.rect));
        }
    }
}

/// Repaint `scene` inside `clip`: the background, then the outlines, the
/// crosshair, and the toolbar, exactly as a full repaint would draw them
/// there.  `toolbar` is the pixmap of `scene.toolbar`.
#[hotpath::measure]
pub fn paint(pixels: &mut [u32], scene: &Scene, toolbar: Option<&Pixmap>, clip: Rect) {
    let (w, h) = (scene.width, scene.height);
    let clip = clip.intersect(&scene.screen());
    if clip.is_empty() {
        return;
    }
    fill(pixels, w, clip, scene.background);
    for outline in [scene.selection, scene.highlight].into_iter().flatten() {
        fill(pixels, w, outline.rect.intersect(&clip), outline.fill);
        for edge in outline.edges() {
            fill(pixels, w, edge.intersect(&clip), outline.border);
        }
    }
    if let Some(target) = scene.target {
        for line in target.lines(w, h) {
            fill(pixels, w, line.intersect(&clip), target.color);
        }
    }
    if let (Some(sprite), Some(pixmap)) = (scene.toolbar, toolbar) {
        render::blit_argb(pixels, w, h, pixmap, sprite.ox, sprite.oy, clip);
    }
}

fn fill(pixels: &mut [u32], width: usize, rect: Rect, color: u32) {
    if rect.is_empty() {
        return;
    }
    for y in rect.y0..rect.y1 {
        pixels[y * width + rect.x0..y * width + rect.x1].fill(color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: usize = 240;
    const H: usize = 160;
    const TINT: u32 = 0x8000_0000;
    const BORDER: u32 = 0xff0a_84ff;

    fn base() -> Scene {
        Scene {
            width: W,
            height: H,
            background: TINT,
            selection: None,
            highlight: None,
            target: None,
            toolbar: None,
        }
    }

    fn selection(x0: usize, y0: usize, x1: usize, y1: usize) -> Option<Outline> {
        Some(Outline {
            rect: Rect::new(x0, y0, x1, y1),
            fill: 0,
            border: BORDER,
            thickness: 1,
        })
    }

    fn highlight(x0: usize, y0: usize, x1: usize, y1: usize) -> Option<Outline> {
        Some(Outline {
            rect: Rect::new(x0, y0, x1, y1),
            fill: 0x3010_2030,
            border: BORDER,
            thickness: 4,
        })
    }

    fn toolbar(ox: i32, generation: u64) -> Option<Sprite> {
        Some(Sprite {
            ox,
            oy: 120,
            width: 60,
            height: 30,
            generation,
        })
    }

    /// A translucent pixmap that differs per generation, so a stale toolbar
    /// left in the buffer shows up as wrong pixels.
    fn pixmap(generation: u64) -> Pixmap {
        let mut pixmap = Pixmap::new(60, 30).unwrap();
        for (i, px) in pixmap
            .data_mut()
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .enumerate()
        {
            let a = if i % 7 == 0 {
                0
            } else {
                96 + (generation as u8) * 40
            };
            let v = ((i as u64 * 13 + generation * 71) % (a as u64 + 1)) as u8;
            px.copy_from_slice(&[v, v / 2, v / 3, a]);
        }
        pixmap
    }

    fn full(scene: &Scene) -> Vec<u32> {
        let mut pixels = vec![0xdead_beef; W * H];
        let pixmap = scene.toolbar.map(|s| pixmap(s.generation));
        paint(&mut pixels, scene, pixmap.as_ref(), scene.screen());
        pixels
    }

    fn update(pixels: &mut [u32], old: Option<&Scene>, new: &Scene) {
        let pixmap = new.toolbar.map(|s| pixmap(s.generation));
        for rect in damage(old, new) {
            paint(pixels, new, pixmap.as_ref(), rect);
        }
    }

    fn scenes() -> Vec<Scene> {
        let clear = Scene {
            background: 0,
            ..base()
        };
        vec![
            base(),
            Scene {
                selection: selection(10, 10, 100, 80),
                ..base()
            },
            Scene {
                selection: selection(14, 8, 104, 78),
                ..base()
            },
            Scene {
                selection: selection(10, 10, 160, 120),
                toolbar: toolbar(90, 1),
                ..base()
            },
            Scene {
                selection: selection(60, 30, 61, 31),
                toolbar: toolbar(90, 2),
                ..base()
            },
            Scene {
                selection: selection(200, 150, 240, 160),
                toolbar: toolbar(-10, 2),
                ..base()
            },
            Scene {
                highlight: highlight(0, 0, 120, 100),
                toolbar: toolbar(90, 2),
                ..clear
            },
            Scene {
                highlight: highlight(100, 20, 240, 160),
                toolbar: toolbar(90, 2),
                ..clear
            },
            Scene {
                target: Some(Crosshair {
                    x: 5,
                    y: 150,
                    radius: 18,
                    thickness: 1,
                    color: BORDER,
                }),
                toolbar: toolbar(90, 3),
                ..clear
            },
            Scene {
                target: Some(Crosshair {
                    x: 230,
                    y: 3,
                    radius: 18,
                    thickness: 2,
                    color: BORDER,
                }),
                ..clear
            },
        ]
    }

    #[test]
    fn repainting_the_damage_matches_a_full_repaint() {
        let scenes = scenes();
        for old in &scenes {
            for new in &scenes {
                let mut pixels = full(old);
                update(&mut pixels, Some(old), new);
                assert!(
                    pixels == full(new),
                    "partial repaint differs\nold: {old:?}\nnew: {new:?}"
                );
            }
        }
    }

    #[test]
    fn a_buffer_two_frames_behind_catches_up_in_one_repaint() {
        // Double buffering: each buffer skips the frame the other one drew.
        let scenes = scenes();
        for window in scenes.windows(3) {
            let mut pixels = full(&window[0]);
            update(&mut pixels, Some(&window[0]), &window[2]);
            assert!(pixels == full(&window[2]), "{window:?}");
        }
    }

    #[test]
    fn a_fresh_buffer_is_painted_whole() {
        let scene = scenes()[3];
        assert_eq!(damage(None, &scene), vec![scene.screen()]);
    }

    #[test]
    fn an_unchanged_frame_has_no_damage() {
        for scene in scenes() {
            assert!(damage(Some(&scene), &scene).is_empty(), "{scene:?}");
        }
    }

    #[test]
    fn nudging_a_large_selection_repaints_only_its_edges() {
        let old = Scene {
            selection: selection(10, 10, 230, 150),
            toolbar: toolbar(90, 1),
            ..base()
        };
        let new = Scene {
            selection: selection(12, 11, 232, 151),
            ..old
        };
        let area: usize = damage(Some(&old), &new).iter().map(Rect::area).sum();
        assert!(area > 0);
        assert!(
            area * 10 < W * H,
            "damaged {area} of {} pixels for a 2 px move",
            W * H
        );
    }

    #[test]
    fn hovering_repaints_only_the_toolbar() {
        let old = Scene {
            toolbar: toolbar(90, 1),
            ..base()
        };
        let new = Scene {
            toolbar: toolbar(90, 2),
            ..old
        };
        assert_eq!(damage(Some(&old), &new), vec![Rect::new(90, 120, 150, 150)]);
    }
}
