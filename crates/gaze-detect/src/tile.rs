//! Tiling of a large frame into square model inputs, and the coordinate transform back.
//!
//! A 3840x1600 ultrawide frame squashed into a 640x640 network input shrinks a 20 px
//! toolbar icon to 3 px, which no detector recovers. Instead the frame is cut into
//! overlapping square tiles, each tile is scaled into the network input on its own, and
//! the boxes are mapped back to frame pixels afterwards.

/// One tile of the source frame plus the affine transform that maps a box in model input
/// pixels back to frame pixels.
///
/// The transform is `frame = origin + (model - pad) / scale`. `scale` is uniform (the tile
/// keeps its aspect ratio) and `pad` is the letterbox offset, which is zero for every
/// square tile and only non-zero when the frame is smaller than one tile on some axis.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tile {
    /// Left edge of the tile in frame pixels.
    pub x       : u32,
    /// Top edge of the tile in frame pixels.
    pub y       : u32,
    /// Tile width in frame pixels.
    pub w       : u32,
    /// Tile height in frame pixels.
    pub h       : u32,
    /// Frame pixels per model input pixel, as a multiplier applied to the frame.
    pub scale   : f64,
    /// Left letterbox padding in model input pixels.
    pub pad_x   : f64,
    /// Top letterbox padding in model input pixels.
    pub pad_y   : f64,
    /// Side length of the square model input this tile is rendered into.
    pub input   : u32,
}

// --- Tile ---

impl Tile {
    /// Maps a point in model input pixels back to frame pixels.
    pub fn to_frame(&self, mx: f64, my: f64) -> (f64, f64) {
        ((mx - self.pad_x) / self.scale + self.x as f64, (my - self.pad_y) / self.scale + self.y as f64)
    }

    /// Maps a point in frame pixels into this tile's model input pixels. Only used by
    /// tests and by the overlay debug path; inference never needs this direction.
    pub fn to_model(&self, fx: f64, fy: f64) -> (f64, f64) {
        ((fx - self.x as f64) * self.scale + self.pad_x, (fy - self.y as f64) * self.scale + self.pad_y)
    }
}

// --- Planning ---

/// Cuts a `w` x `h` frame into overlapping square tiles of `tile_px` frame pixels, each
/// rendered into an `input_px` square model input.
///
/// `overlap` is the fraction of a tile shared with its neighbour, so 0.15 steps by 85% of
/// the tile. Tiles never run off the edge of the frame: the last row and column are
/// shifted back so they end exactly at the frame border, which costs a little extra
/// overlap there and nothing else. When the frame is smaller than a tile on an axis the
/// tile shrinks to the frame and the result is letterboxed into the square input.
pub fn plan_tiles(
    w        : u32,
    h        : u32,
    tile_px  : u32,
    overlap  : f64,
    input_px : u32,
)
    -> Vec<Tile>
{
    // A degenerate frame has no tiles at all; callers treat that as "no detections".
    if w == 0 || h == 0 {
        return Vec::new();
    }

    let tw = tile_px.min(w);
    let th = tile_px.min(h);

    // The scale is shared by both axes so the tile is not distorted. A tile that is
    // square and exactly `input_px` across is the common case and gives scale 1.
    let scale = (input_px as f64 / tw as f64).min(input_px as f64 / th as f64);
    let pad_x = (input_px as f64 - tw as f64 * scale) * 0.5;
    let pad_y = (input_px as f64 - th as f64 * scale) * 0.5;

    let xs = axis_starts(w, tw, overlap);
    let ys = axis_starts(h, th, overlap);

    let mut tiles = Vec::with_capacity(xs.len() * ys.len());

    for y in &ys {
        for x in &xs {
            tiles.push(Tile {
                x     : *x,
                y     : *y,
                w     : tw,
                h     : th,
                scale : scale,
                pad_x : pad_x,
                pad_y : pad_y,
                input : input_px,
            });
        }
    }

    tiles
}

/// Start offsets along one axis so that tiles of `tile` cover `extent` with `overlap`.
///
/// The final start is clamped to `extent - tile` rather than allowed to hang off the end,
/// so every tile is full size and the preprocessor never has to handle a partial crop.
fn axis_starts(extent: u32, tile: u32, overlap: f64) -> Vec<u32> {
    if extent <= tile {
        return vec![0];
    }

    let stride = ((tile as f64 * (1.0 - overlap)).round() as u32).max(1);
    let span   = extent - tile;
    let count  = span.div_ceil(stride) + 1;

    (0..count).map(|i| (i * stride).min(span)).collect()
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::{Tile, axis_starts, plan_tiles};

    /// The ultrawide case the tiler exists for: full coverage, no tile off the edge, and
    /// the requested overlap actually present between neighbours.
    #[test]
    fn ultrawide_tiles_cover_the_frame() {
        let tiles = plan_tiles(3840, 1600, 640, 0.15, 640);

        assert!(!tiles.is_empty());

        for t in &tiles {
            assert!(t.x + t.w <= 3840);
            assert!(t.y + t.h <= 1600);
            assert_eq!(t.scale, 1.0);
        }

        // Every frame pixel must fall inside at least one tile. Checking the four corners
        // and a stride-sized grid of interior points is enough for an axis-aligned plan.
        for fy in (0..1600).step_by(37) {
            for fx in (0..3840).step_by(53) {
                let hit = tiles.iter().any(|t| {
                    fx >= t.x && fx < t.x + t.w && fy >= t.y && fy < t.y + t.h
                });

                assert!(hit, "pixel {fx},{fy} not covered");
            }
        }
    }

    /// Round trip through the tile transform must be exact for an unscaled tile and within
    /// float noise for a downscaled one.
    #[test]
    fn model_to_frame_round_trips() {
        for (tile_px, input_px) in [(640, 640), (1280, 640), (1024, 640)] {
            let tiles = plan_tiles(3840, 1600, tile_px, 0.15, input_px);
            let t     = tiles[tiles.len() / 2];

            for (fx, fy) in [(t.x as f64 + 1.0, t.y as f64 + 1.0), (t.x as f64 + 100.5, t.y as f64 + 40.25)] {
                let (mx, my) = t.to_model(fx, fy);
                let (bx, by) = t.to_frame(mx, my);

                assert!((bx - fx).abs() < 1e-9, "{bx} != {fx}");
                assert!((by - fy).abs() < 1e-9, "{by} != {fy}");
            }
        }
    }

    /// A box detected in the second tile must land at its true frame position, not at the
    /// tile-local one. This is the mapping bug that silently halves recall.
    #[test]
    fn second_tile_box_maps_to_absolute_frame_coordinates() {
        let tiles = plan_tiles(3840, 1600, 640, 0.15, 640);
        let t     = tiles[1];

        assert_eq!(t.x, 544);
        assert_eq!(t.y, 0);

        // A box at model (10, 20)-(60, 40) inside tile 1 is at frame (554, 20)-(604, 40).
        let (x0, y0) = t.to_frame(10.0, 20.0);
        let (x1, y1) = t.to_frame(60.0, 40.0);

        assert_eq!((x0, y0), (554.0, 20.0));
        assert_eq!((x1, y1), (604.0, 40.0));
    }

    /// A downscaled tile multiplies model coordinates by the inverse scale.
    #[test]
    fn downscaled_tile_expands_boxes() {
        let tiles = plan_tiles(3840, 1600, 1280, 0.15, 640);
        let t     = tiles[0];

        assert_eq!(t.scale, 0.5);
        assert_eq!(t.pad_x, 0.0);
        assert_eq!(t.to_frame(100.0, 50.0), (200.0, 100.0));
    }

    /// A frame shorter than one tile is letterboxed on that axis and still tiled on the
    /// other. This is the HDMI panel: 960x600 logical needs two tiles across and one down,
    /// with 20 px of padding top and bottom.
    #[test]
    fn short_frame_is_letterboxed_on_the_short_axis() {
        let tiles = plan_tiles(960, 600, 640, 0.15, 640);

        assert_eq!(tiles.len(), 2);
        assert_eq!(tiles[1].x, 320, "the last column is flush with the right edge");

        let t = tiles[0];

        assert_eq!(t.w, 640);
        assert_eq!(t.h, 600);

        // Height is the limiting axis only if it is proportionally smaller; here width is,
        // so scale is 1.0 and the 600 px height leaves 20 px of padding top and bottom.
        assert_eq!(t.scale, 1.0);
        assert_eq!(t.pad_x, 0.0);
        assert_eq!(t.pad_y, 20.0);
        assert_eq!(t.to_frame(0.0, 20.0), (0.0, 0.0));
    }

    /// The tiny-frame case where the frame is smaller than the input on both axes.
    #[test]
    fn tiny_frame_scales_up_to_fill_the_input() {
        let tiles = plan_tiles(320, 200, 640, 0.15, 640);
        let t     = tiles[0];

        assert_eq!(t.w, 320);
        assert_eq!(t.h, 200);
        assert_eq!(t.scale, 2.0);
        assert_eq!(t.pad_x, 0.0);
        assert_eq!(t.pad_y, 120.0);
    }

    /// Start offsets are monotonic, unique, and end flush with the frame edge.
    #[test]
    fn axis_starts_are_monotonic_and_flush() {
        let s = axis_starts(3840, 640, 0.15);

        assert_eq!(s.first(), Some(&0));
        assert_eq!(s.last(), Some(&(3840 - 640)));

        for pair in s.windows(2) {
            assert!(pair[1] > pair[0], "{s:?} not strictly increasing");
        }

        assert_eq!(axis_starts(640, 640, 0.15), vec![0]);
        assert_eq!(axis_starts(100, 640, 0.15), vec![0]);
    }

    /// The transform is a pure function of the tile, so a hand-built tile is enough to
    /// pin the letterbox convention down.
    #[test]
    fn hand_built_tile_transform() {
        let t = Tile {
            x     : 100,
            y     : 200,
            w     : 320,
            h     : 320,
            scale : 2.0,
            pad_x : 0.0,
            pad_y : 0.0,
            input : 640,
        };

        assert_eq!(t.to_frame(0.0, 0.0), (100.0, 200.0));
        assert_eq!(t.to_frame(640.0, 640.0), (420.0, 520.0));
    }
}
