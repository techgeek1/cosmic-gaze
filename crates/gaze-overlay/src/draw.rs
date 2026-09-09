//! Turning an [`OverlayState`] into pixels in a `wl_shm` buffer.
//!
//! The state is first converted into a flat list of [`Item`]s in buffer pixels. That
//! indirection exists so the repaint rectangle can be computed from the item bounds
//! before anything is drawn: the overlay only clears and damages the union of what was in
//! the buffer last time and what is about to go in, which keeps a 3840x1600 surface from
//! being memset sixty times a second for the sake of a 24 px ring.

use tiny_skia::{
    BlendMode, Color, FillRule, LineCap, Paint, PathBuilder, Pixmap, PixmapMut, Rect as SkRect,
    Stroke, Transform,
};

use gaze_core::GlobalPx;

use crate::font;
use crate::mapping::OutputMapping;
use crate::state::OverlayState;

/// Radius of the gaze ring in logical pixels. The ring reads as roughly 24 px across,
/// which is large enough to find on a 38 inch panel without hiding the target under it.
const GAZE_RADIUS_PX: f64 = 12.0;

/// Stroke width of the gaze ring in logical pixels.
const GAZE_STROKE_PX: f64 = 3.0;

/// Radius of the dot at the centre of the gaze ring, in logical pixels.
const GAZE_DOT_PX: f64 = 2.0;

/// Stroke width of the highlight box in logical pixels.
const HIGHLIGHT_STROKE_PX: f64 = 2.0;

/// Half length of a truth cross arm in logical pixels.
const TRUTH_ARM_PX: f64 = 7.0;

/// Stroke width of the truth cross in logical pixels.
const TRUTH_STROKE_PX: f64 = 2.0;

/// Size of one font pixel in logical pixels. 2 gives a 10x14 glyph, legible at arm's
/// length on all three panels.
const LABEL_PIXEL_PX: f64 = 2.0;

/// Padding between the label text and the edge of its backing plate, in logical pixels.
const LABEL_PAD_PX: f64 = 3.0;

/// Gaze ring colour, cyan.
const GAZE_COLOR: [u8; 4] = [80, 200, 255, 210];

/// Highlight stroke colour, amber.
const HIGHLIGHT_COLOR: [u8; 4] = [255, 190, 60, 235];

/// Highlight interior colour. Faint enough to read text through.
const HIGHLIGHT_FILL: [u8; 4] = [255, 190, 60, 28];

/// Truth marker colour, magenta, deliberately unlike the gaze ring.
const TRUTH_COLOR: [u8; 4] = [255, 80, 200, 230];

/// Label text colour.
const LABEL_COLOR: [u8; 4] = [255, 255, 255, 240];

/// Label backing plate colour.
const LABEL_PLATE: [u8; 4] = [0, 0, 0, 165];

/// An integer rectangle in buffer pixels. Repaint and damage bookkeeping is done in whole
/// pixels because `wl_surface.damage_buffer` is, and because a half covered pixel left
/// uncleared shows up as a smear.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelBox {
    pub x : i32,
    pub y : i32,
    pub w : i32,
    pub h : i32,
}

/// One primitive to draw, already in buffer pixels.
#[derive(Clone, Debug)]
pub enum Item {
    /// The gaze marker ring.
    Ring {
        cx     : f32,
        cy     : f32,
        radius : f32,
        stroke : f32,
        color  : [u8; 4],
    },
    /// The filled dot at the centre of the gaze marker.
    Dot {
        cx     : f32,
        cy     : f32,
        radius : f32,
        color  : [u8; 4],
    },
    /// The highlighted candidate box: a stroke plus a faint interior.
    Box {
        x      : f32,
        y      : f32,
        w      : f32,
        h      : f32,
        stroke : f32,
        color  : [u8; 4],
        fill   : [u8; 4],
    },
    /// The ground-truth marker.
    Cross {
        cx     : f32,
        cy     : f32,
        arm    : f32,
        stroke : f32,
        color  : [u8; 4],
    },
    /// A text label on a backing plate, `x`/`y` at the top left of the plate.
    Label {
        x     : f32,
        y     : f32,
        /// Size of one font pixel in buffer pixels.
        px    : f32,
        text  : String,
    },
    /// The pointer look's dot: a filled disc with a halo ring of `halo` width outside
    /// it, so it stays visible on a background the colour of the disc.
    Marker {
        cx         : f32,
        cy         : f32,
        radius     : f32,
        halo       : f32,
        color      : [u8; 4],
        halo_color : [u8; 4],
    },
    /// The pointer look's highlight: a rounded rectangle with a faint interior, a
    /// stroke, and a halo either side of the stroke.
    RoundBox {
        x          : f32,
        y          : f32,
        w          : f32,
        h          : f32,
        radius     : f32,
        stroke     : f32,
        halo       : f32,
        color      : [u8; 4],
        fill       : [u8; 4],
        halo_color : [u8; 4],
    },
}

// --- Api ---

/// Converts the overlay state into draw items for one output.
///
/// Items are emitted whether or not they land on this output; a marker that straddles the
/// seam has to be drawn clipped on both surfaces rather than dropped by either. Callers
/// clip the resulting bounds to the surface, and an item entirely off the surface
/// contributes an empty clipped box and costs nothing.
pub fn scene(state: &OverlayState, map: &OutputMapping) -> Vec<Item> {
    let mut items = Vec::new();

    // The highlight goes down first so the gaze ring stays readable on top of it.
    if let Some(r) = state.highlight {
        let (x, y, w, h) = map.buffer_rect(r);

        items.push(Item::Box {
            x      : x,
            y      : y,
            w      : w,
            h      : h,
            stroke : map.buffer_len(HIGHLIGHT_STROKE_PX),
            color  : HIGHLIGHT_COLOR,
            fill   : HIGHLIGHT_FILL,
        });
    }

    if let Some(t) = state.truth {
        let (cx, cy) = map.buffer(t);

        items.push(Item::Cross {
            cx     : cx,
            cy     : cy,
            arm    : map.buffer_len(TRUTH_ARM_PX),
            stroke : map.buffer_len(TRUTH_STROKE_PX),
            color  : TRUTH_COLOR,
        });
    }

    if let Some(g) = state.gaze {
        let (cx, cy) = map.buffer(g);

        items.push(Item::Ring {
            cx     : cx,
            cy     : cy,
            radius : map.buffer_len(GAZE_RADIUS_PX),
            stroke : map.buffer_len(GAZE_STROKE_PX),
            color  : GAZE_COLOR,
        });
        items.push(Item::Dot {
            cx     : cx,
            cy     : cy,
            radius : map.buffer_len(GAZE_DOT_PX),
            color  : GAZE_COLOR,
        });
    }

    if let Some(text) = state.label.as_deref().filter(|t| !t.is_empty()) {
        items.push(label_item(text, state, map));
    }

    items
}

/// Renders one output's worth of overlay content into a standalone pixmap, transparent
/// everywhere nothing is drawn (or filled with the state's background, when it has one).
///
/// This is the same drawing path the compositor gets, minus the `wl_shm` plumbing and the
/// byte order fixup, so it is the cheap way to check what the overlay would put on a
/// given output without a compositor or a pair of eyes. Returns `None` when the output's
/// buffer size is degenerate.
pub fn render(state: &OverlayState, map: &OutputMapping) -> Option<Pixmap> {
    let (w, h)     = map.buffer_size();
    let mut pixmap = Pixmap::new(w, h)?;
    let whole      = PixelBox { x: 0, y: 0, w: w as i32, h: h as i32 };

    clear(&mut pixmap.as_mut(), whole, state.background);
    draw(&mut pixmap.as_mut(), &scene(state, map));

    Some(pixmap)
}

/// Bounding box of every item, in buffer pixels, or `None` when there is nothing to draw.
pub fn bounds(items: &[Item]) -> Option<PixelBox> {
    items.iter().map(item_bounds).reduce(|a, b| a.union(b))
}

/// Clears `area` to `fill`, or to fully transparent when there is no background. A plain
/// memset per row beats going through tiny-skia's blender for what is always an axis
/// aligned solid fill.
///
/// The bytes go down in tiny-skia's premultiplied RGBA order, so a caller that is filling
/// a `wl_shm` buffer still has to run [`rgba_to_argb`] over the same area afterwards.
pub fn clear(pixmap: &mut PixmapMut, area: PixelBox, fill: Option<[u8; 4]>) {
    let Some(color) = fill else {
        let stride = pixmap.width() as usize * 4;
        let data   = pixmap.data_mut();

        for row in area.y..area.y + area.h {
            let start = row as usize * stride + area.x as usize * 4;

            data[start..start + area.w as usize * 4].fill(0);
        }

        return;
    };

    fill_solid(pixmap, area, premultiply(color));
}

/// Draws every item into the pixmap. The caller must have cleared at least the union of
/// the item bounds first; nothing here is expected to overwrite stale pixels on its own.
pub fn draw(pixmap: &mut PixmapMut, items: &[Item]) {
    for item in items {
        match item {
            Item::Ring { cx, cy, radius, stroke, color } => {
                let Some(path) = PathBuilder::from_circle(*cx, *cy, *radius) else {
                    continue;
                };

                pixmap.stroke_path(
                    &path,
                    &paint(*color),
                    &stroke_of(*stroke),
                    Transform::identity(),
                    None,
                );
            }

            Item::Dot { cx, cy, radius, color } => {
                let Some(path) = PathBuilder::from_circle(*cx, *cy, *radius) else {
                    continue;
                };

                pixmap.fill_path(
                    &path,
                    &paint(*color),
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );
            }

            Item::Box { x, y, w, h, stroke, color, fill } => {
                let Some(rect) = SkRect::from_xywh(*x, *y, *w, *h) else {
                    continue;
                };

                pixmap.fill_rect(rect, &paint(*fill), Transform::identity(), None);

                let Some(path) = PathBuilder::from_rect(rect).stroke(&stroke_of(*stroke), 1.0)
                else {
                    continue;
                };

                pixmap.fill_path(
                    &path,
                    &paint(*color),
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );
            }

            Item::Cross { cx, cy, arm, stroke, color } => {
                let mut pb = PathBuilder::new();

                pb.move_to(cx - arm, *cy);
                pb.line_to(cx + arm, *cy);
                pb.move_to(*cx, cy - arm);
                pb.line_to(*cx, cy + arm);

                let Some(path) = pb.finish() else {
                    continue;
                };

                pixmap.stroke_path(
                    &path,
                    &paint(*color),
                    &stroke_of(*stroke),
                    Transform::identity(),
                    None,
                );
            }

            Item::Label { x, y, px, text } => {
                draw_label(pixmap, *x, *y, *px, text);
            }

            Item::Marker { cx, cy, radius, halo, color, halo_color } => {
                // The halo is a larger disc underneath rather than a stroke, so the
                // two antialiased edges do not leave a seam between them.
                for (r, c) in [(radius + halo, halo_color), (*radius, color)] {
                    let Some(path) = PathBuilder::from_circle(*cx, *cy, r) else {
                        continue;
                    };

                    pixmap.fill_path(
                        &path,
                        &paint(*c),
                        FillRule::Winding,
                        Transform::identity(),
                        None,
                    );
                }
            }

            Item::RoundBox { x, y, w, h, radius, stroke, halo, color, fill, halo_color } => {
                let Some(path) = rounded_rect(*x, *y, *w, *h, *radius) else {
                    continue;
                };

                pixmap.fill_path(
                    &path,
                    &paint(*fill),
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );

                // A wide halo stroke under the accent stroke leaves `halo` showing on
                // each side of it.
                for (width, c) in [(stroke + halo * 2.0, halo_color), (*stroke, color)] {
                    pixmap.stroke_path(
                        &path,
                        &paint(*c),
                        &stroke_of(width),
                        Transform::identity(),
                        None,
                    );
                }
            }
        }
    }
}

/// Rewrites `area` from tiny-skia's premultiplied RGBA byte order into the premultiplied
/// BGRA that `wl_shm`'s `Argb8888` means on a little endian machine, in place.
///
/// Only the repainted rectangle is touched, so this costs the same as the repaint rather
/// than the whole surface.
pub fn rgba_to_argb(pixmap: &mut PixmapMut, area: PixelBox) {
    let stride = pixmap.width() as usize * 4;
    let data   = pixmap.data_mut();

    for row in area.y..area.y + area.h {
        let start = row as usize * stride + area.x as usize * 4;
        let end   = start + area.w as usize * 4;

        for px in data[start..end].chunks_exact_mut(4) {
            px.swap(0, 2);
        }
    }
}

// --- PixelBox ---

impl PixelBox {
    /// The empty box at the origin.
    pub const EMPTY: PixelBox = PixelBox { x: 0, y: 0, w: 0, h: 0 };

    /// Smallest integer box covering the given float rectangle, grown by one pixel on
    /// every side to swallow antialiased edges.
    pub fn around(x: f32, y: f32, w: f32, h: f32) -> PixelBox {
        let x0 = (x.floor() as i32).saturating_sub(1);
        let y0 = (y.floor() as i32).saturating_sub(1);
        let x1 = ((x + w).ceil() as i32).saturating_add(1);
        let y1 = ((y + h).ceil() as i32).saturating_add(1);

        PixelBox { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
    }

    /// True when the box covers no pixels.
    pub fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    /// Smallest box covering both. An empty operand is ignored rather than dragging the
    /// result out to the origin.
    pub fn union(self, other: PixelBox) -> PixelBox {
        if self.is_empty() {
            return other;
        }

        if other.is_empty() {
            return self;
        }

        let x0 = self.x.min(other.x);
        let y0 = self.y.min(other.y);
        let x1 = (self.x + self.w).max(other.x + other.w);
        let y1 = (self.y + self.h).max(other.y + other.h);

        PixelBox { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
    }

    /// The part of this box inside a `width` by `height` buffer. Returns
    /// [`PixelBox::EMPTY`] when nothing is left.
    pub fn clip_to(self, width: u32, height: u32) -> PixelBox {
        let x0 = self.x.max(0);
        let y0 = self.y.max(0);
        let x1 = (self.x + self.w).min(width as i32);
        let y1 = (self.y + self.h).min(height as i32);

        if x1 <= x0 || y1 <= y0 {
            return PixelBox::EMPTY;
        }

        PixelBox { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
    }
}

// --- Internals ---

/// Bounding box of a single item.
fn item_bounds(item: &Item) -> PixelBox {
    match item {
        Item::Ring { cx, cy, radius, stroke, .. } => {
            let r = radius + stroke * 0.5;

            PixelBox::around(cx - r, cy - r, r * 2.0, r * 2.0)
        }

        Item::Dot { cx, cy, radius, .. } => {
            PixelBox::around(cx - radius, cy - radius, radius * 2.0, radius * 2.0)
        }

        Item::Box { x, y, w, h, stroke, .. } => {
            let s = stroke * 0.5;

            PixelBox::around(x - s, y - s, w + stroke, h + stroke)
        }

        Item::Cross { cx, cy, arm, stroke, .. } => {
            let r = arm + stroke * 0.5;

            PixelBox::around(cx - r, cy - r, r * 2.0, r * 2.0)
        }

        Item::Label { x, y, px, text } => {
            let (w, h) = label_size(text, *px);

            PixelBox::around(*x, *y, w, h)
        }

        Item::Marker { cx, cy, radius, halo, .. } => {
            let r = radius + halo;

            PixelBox::around(cx - r, cy - r, r * 2.0, r * 2.0)
        }

        Item::RoundBox { x, y, w, h, stroke, halo, .. } => {
            let s = stroke * 0.5 + halo;

            PixelBox::around(x - s, y - s, w + s * 2.0, h + s * 2.0)
        }
    }
}

/// Places the label. It sits just above the highlight box when there is one, otherwise
/// just below the gaze ring, and is pushed back inside the box when there is no room
/// above it (a highlight touching the top edge of the output).
fn label_item(text: &str, state: &OverlayState, map: &OutputMapping) -> Item {
    let px      = map.buffer_len(LABEL_PIXEL_PX);
    let (_, h)  = label_size(text, px);
    let gap     = map.buffer_len(LABEL_PAD_PX);

    let (x, y) = {
        if let Some(r) = state.highlight {
            let (bx, by, ..) = map.buffer_rect(r);
            let above        = by - h - gap;

            (bx, if above >= 0.0 { above } else { by + gap })
        }
        else {
            let anchor = state.gaze.unwrap_or(GlobalPx { x: map.logical.x, y: map.logical.y });
            let (gx, gy) = map.buffer(anchor);

            (gx + map.buffer_len(GAZE_RADIUS_PX), gy + map.buffer_len(GAZE_RADIUS_PX))
        }
    };

    Item::Label {
        x    : x,
        y    : y,
        px   : px,
        text : text.to_string(),
    }
}

/// Size of a label including its backing plate, in buffer pixels.
fn label_size(text: &str, px: f32) -> (f32, f32) {
    let (tw, th) = font::text_size(text);
    let pad      = px * 2.0;

    (tw as f32 * px + pad * 2.0, th as f32 * px + pad * 2.0)
}

/// Draws the backing plate through tiny-skia, then the glyph pixels as direct writes.
/// Every glyph pixel is an axis aligned `px` by `px` square, so going through the path
/// rasteriser for a few hundred of them would be pure overhead.
fn draw_label(pixmap: &mut PixmapMut, x: f32, y: f32, px: f32, text: &str) {
    let (w, h) = label_size(text, px);
    let pad    = px * 2.0;

    if let Some(rect) = SkRect::from_xywh(x, y, w, h) {
        pixmap.fill_rect(rect, &paint(LABEL_PLATE), Transform::identity(), None);
    }

    let origin_x = x + pad;
    let origin_y = y + pad;
    let width    = pixmap.width();
    let height   = pixmap.height();
    let premul   = premultiply(LABEL_COLOR);

    font::for_each_pixel(text, |col, row| {
        let cell = PixelBox::around(
            origin_x + col as f32 * px,
            origin_y + row as f32 * px,
            px,
            px,
        );

        // `around` grows by a pixel for antialiasing, which glyph cells do not want.
        let cell = PixelBox {
            x : cell.x + 1,
            y : cell.y + 1,
            w : cell.w - 2,
            h : cell.h - 2,
        };

        fill_solid(pixmap, cell.clip_to(width, height), premul);
    });
}

/// A rounded rectangle path. The radius is clamped to half the shorter side, so a
/// highlight on a thin element degrades to a pill rather than to a broken path.
fn rounded_rect(x: f32, y: f32, w: f32, h: f32, radius: f32) -> Option<tiny_skia::Path> {
    let r = radius.max(0.0).min(w * 0.5).min(h * 0.5);

    if r <= 0.0 {
        return SkRect::from_xywh(x, y, w, h).map(PathBuilder::from_rect);
    }

    // Circular arcs as cubics: the standard control distance for a quarter circle.
    let k  = 0.5523 * r;
    let x1 = x + w;
    let y1 = y + h;

    let mut pb = PathBuilder::new();

    pb.move_to(x + r, y);
    pb.line_to(x1 - r, y);
    pb.cubic_to(x1 - r + k, y, x1, y + r - k, x1, y + r);
    pb.line_to(x1, y1 - r);
    pb.cubic_to(x1, y1 - r + k, x1 - r + k, y1, x1 - r, y1);
    pb.line_to(x + r, y1);
    pb.cubic_to(x + r - k, y1, x, y1 - r + k, x, y1 - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();

    pb.finish()
}

/// Writes a premultiplied RGBA colour over every pixel of an already clipped box.
fn fill_solid(pixmap: &mut PixmapMut, area: PixelBox, premul: [u8; 4]) {
    if area.is_empty() {
        return;
    }

    let stride = pixmap.width() as usize * 4;
    let data   = pixmap.data_mut();

    for row in area.y..area.y + area.h {
        let start = row as usize * stride + area.x as usize * 4;
        let end   = start + area.w as usize * 4;

        for px in data[start..end].chunks_exact_mut(4) {
            px.copy_from_slice(&premul);
        }
    }
}

/// Premultiplies a straight-alpha colour, matching what tiny-skia stores in a pixmap.
fn premultiply(c: [u8; 4]) -> [u8; 4] {
    let a = u32::from(c[3]);
    let m = |v: u8| ((u32::from(v) * a + 127) / 255) as u8;

    [m(c[0]), m(c[1]), m(c[2]), c[3]]
}

/// An antialiased source-over paint in the given straight-alpha colour.
fn paint<'a>(c: [u8; 4]) -> Paint<'a> {
    let mut paint = Paint::default();

    paint.set_color(Color::from_rgba8(c[0], c[1], c[2], c[3]));
    paint.anti_alias = true;
    paint.blend_mode = BlendMode::SourceOver;

    paint
}

/// A butt-capped stroke of the given width.
fn stroke_of(width: f32) -> Stroke {
    Stroke {
        width      : width.max(1.0),
        line_cap   : LineCap::Butt,
        ..Stroke::default()
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use gaze_core::Rect;

    use super::*;

    /// A HiDPI output, so the tests exercise the scale multiplication rather than the
    /// identity. Nothing on this desk runs at 2, but the arithmetic has to be right.
    fn hidpi() -> OutputMapping {
        OutputMapping::new("HIDPI-1", Rect { x: 1506.0, y: 1600.0, w: 960.0, h: 600.0 }, 2)
    }

    #[test]
    fn empty_state_draws_nothing() {
        assert!(scene(&OverlayState::default(), &hidpi()).is_empty());
        assert_eq!(bounds(&[]), None);
    }

    #[test]
    fn gaze_marker_bounds_are_centred_and_scaled() {
        let state = OverlayState {
            gaze : Some(GlobalPx { x: 1986.0, y: 1900.0 }),
            ..OverlayState::default()
        };

        let items = scene(&state, &hidpi());
        let b     = bounds(&items).expect("a gaze point produces items");

        // Centre in buffer px is (960, 600); radius 12 logical px at scale 2 is 24 buffer
        // px, plus half of a 6 px stroke, plus the one pixel antialias margin.
        assert_eq!(b.x + b.w / 2, 960);
        assert_eq!(b.y + b.h / 2, 600);
        assert_eq!(b.w          , 2 * (24 + 3 + 1));
    }

    #[test]
    fn clipping_drops_a_marker_that_is_on_another_output() {
        let state = OverlayState {
            gaze : Some(GlobalPx { x: 100.0, y: 200.0 }),
            ..OverlayState::default()
        };

        let map   = hidpi();
        let items = scene(&state, &map);
        let (w, h) = map.buffer_size();

        assert!(bounds(&items).expect("items exist").clip_to(w, h).is_empty());
    }

    #[test]
    fn a_background_covers_every_pixel_under_the_markers() {
        let map   = hidpi();
        let state = OverlayState {
            gaze       : Some(GlobalPx { x: 1986.0, y: 1900.0 }),
            background : Some([255, 255, 255, 255]),
            ..OverlayState::default()
        };

        let pixmap = render(&state, &map).expect("a rendered surface");

        // Every pixel is opaque: the corners are bare background, the centre is the ring
        // drawn over it. A transparent pixel anywhere would be a hole onto the desktop,
        // which is exactly what a recording session must not have.
        assert!(pixmap.data().chunks_exact(4).all(|px| px[3] == 255));

        // A far corner is the background colour and nothing else.
        assert_eq!(&pixmap.data()[0..4], &[255, 255, 255, 255]);

        // Without a background the same state leaves that corner transparent.
        let bare = render(&OverlayState { background: None, ..state }, &map)
            .expect("a rendered surface");

        assert_eq!(&bare.data()[0..4], &[0, 0, 0, 0]);
    }

    #[test]
    fn union_ignores_empty_boxes() {
        let a = PixelBox { x: 10, y: 10, w: 5, h: 5 };

        assert_eq!(a.union(PixelBox::EMPTY), a);
        assert_eq!(PixelBox::EMPTY.union(a), a);
    }

    #[test]
    fn union_covers_both_operands() {
        let a = PixelBox { x: 0, y: 0, w: 4, h: 4 };
        let b = PixelBox { x: 10, y: 2, w: 2, h: 20 };

        assert_eq!(a.union(b), PixelBox { x: 0, y: 0, w: 12, h: 22 });
    }

    #[test]
    fn clip_to_trims_negative_origins() {
        let a = PixelBox { x: -5, y: -5, w: 20, h: 20 };

        assert_eq!(a.clip_to(10, 10), PixelBox { x: 0, y: 0, w: 10, h: 10 });
    }

    #[test]
    fn premultiply_scales_colour_by_alpha() {
        assert_eq!(premultiply([255, 255, 255, 255]), [255, 255, 255, 255]);
        assert_eq!(premultiply([255, 0, 0, 0])      , [0, 0, 0, 0]);
        assert_eq!(premultiply([200, 100, 50, 128]) , [100, 50, 25, 128]);
    }

    #[test]
    fn argb_swizzle_swaps_red_and_blue_only_inside_the_area() {
        let mut data = vec![0u8; 4 * 4 * 4];
        let mut pm   = PixmapMut::from_bytes(&mut data, 4, 4).expect("4x4 pixmap");

        // Mark every pixel red in tiny-skia's RGBA order.
        for px in pm.data_mut().chunks_exact_mut(4) {
            px.copy_from_slice(&[255, 0, 0, 255]);
        }

        rgba_to_argb(&mut pm, PixelBox { x: 1, y: 1, w: 2, h: 2 });

        // Inside the area red has moved to the third byte, outside it has not.
        assert_eq!(&data[(4 + 1) * 4..][..4], &[0, 0, 255, 255]);
        assert_eq!(&data[0..4]                  , &[255, 0, 0, 255]);
    }
}
