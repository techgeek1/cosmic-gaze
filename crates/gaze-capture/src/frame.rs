//! Captured pixels and the cheap frame-diff trigger that decides whether a new frame is
//! worth handing to the detector.

use gaze_core::{GlobalPx, Rect};

/// Side of the square block the frame differ averages over, in buffer pixels. Eight is
/// small enough that a single toolbar button changing state still trips a block, and large
/// enough that a 3840x1600 diff touches only 480x200 accumulator slots.
const BLOCK_PX: u32 = 8;

/// Mean per-channel absolute difference, in 0-255 units, above which a block counts as
/// changed. Four survives dithering and video-scaler noise but catches a hover highlight.
const BLOCK_THRESHOLD: u32 = 4;

/// One enabled output as the compositor currently describes it.
///
/// `logical` is the global logical-pixel rectangle the output occupies (what
/// `zxdg_output_v1` reports, and the space every box in this workspace lives in).
/// `physical_w`/`physical_h` are the capture buffer's dimensions, which are the mode
/// dimensions, not the logical ones: HDMI-A-1 at scale 2 is 1920x1200 physical and
/// 960x600 logical.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputInfo {
    /// Connector name from `wl_output.name`, for example `"DP-1"` or `"HDMI-A-1"`.
    pub name       : String,
    pub logical    : Rect,
    /// Physical pixels per logical pixel, derived from the buffer and logical widths so it
    /// reflects fractional scaling rather than the integer `wl_output.scale`.
    pub scale      : f64,
    pub physical_w : u32,
    pub physical_h : u32,
}

/// One captured output image, always RGBA8 with a tightly packed `width * 4` stride.
///
/// The cursor is never composited in: the capture session is created without
/// `paint_cursors`, so a detector never sees a pointer-shaped element.
#[derive(Clone, Debug)]
pub struct Frame {
    /// Connector name of the output this came from.
    pub output  : String,
    /// Where the output sits in global logical pixels, sampled at capture time.
    pub logical : Rect,
    /// Buffer width in physical pixels.
    pub width   : u32,
    /// Buffer height in physical pixels.
    pub height  : u32,
    /// `width * height * 4` bytes, RGBA8, top row first.
    pub rgba    : Vec<u8>,
    /// `CLOCK_MONOTONIC` seconds. This is the compositor's `presentation_time` for the
    /// frame when it sent one, otherwise the client-side clock reading taken when the
    /// `ready` event arrived. The two share a clock domain, so mixing them is safe.
    pub t_s     : f64,
}

// --- OutputInfo ---

impl OutputInfo {
    /// Maps a point given in this output's capture-buffer pixels to global logical pixels.
    ///
    /// Buffer pixels are physical, which is the space `ext_image_copy_capture` reports
    /// cursor positions and frame damage in, so the offset is scaled down before being
    /// added to the logical origin. On a scale-1 output this is a pure translation.
    ///
    /// The two axes are scaled independently rather than by the single `scale` field.
    /// cosmic-comp rounds the logical size it reports, so on HDMI-A-1 (1920x1200 physical,
    /// 1670x1043 logical) the width and height ratios differ by 0.07%; using the width
    /// ratio for both would put the bottom edge 0.75 px out.
    pub fn buffer_to_global(&self, x: f64, y: f64) -> GlobalPx {
        // A degenerate output cannot come out of the compositor, but dividing by zero
        // would poison every downstream coordinate, so fall back to a pure translation.
        let sx = if self.physical_w > 0 { self.logical.w / f64::from(self.physical_w) } else { 1.0 };
        let sy = if self.physical_h > 0 { self.logical.h / f64::from(self.physical_h) } else { 1.0 };

        GlobalPx {
            x : self.logical.x + x * sx,
            y : self.logical.y + y * sy,
        }
    }
}

// --- Frame ---

impl Frame {
    /// Physical pixels per logical pixel for this frame, matching `OutputInfo::scale`.
    ///
    /// Returns 1.0 for a degenerate zero-width logical rectangle rather than a NaN, so
    /// callers mapping detector boxes back to global coordinates cannot poison their
    /// arithmetic.
    pub fn scale(&self) -> f64 {
        if self.logical.w <= 0.0 {
            return 1.0;
        }

        f64::from(self.width) / self.logical.w
    }
}

// --- Frame diff ---

/// Fraction of 8x8 blocks whose mean absolute RGB delta exceeds a small threshold.
///
/// This is the trigger for re-running detection: a static desktop scores ~0, a scrolling
/// window scores high. Frames of differing size score 1.0, because a resolution change is
/// exactly the kind of event that invalidates every cached element box.
///
/// Cost is one pass over both buffers with no allocation beyond a row of block
/// accumulators, so it is cheap enough to run on every captured frame.
pub fn changed_fraction(a: &Frame, b: &Frame) -> f32 {
    // A size change invalidates everything downstream, so do not pretend to compare.
    if a.width != b.width || a.height != b.height {
        return 1.0;
    }

    if a.width == 0 || a.height == 0 {
        return 0.0;
    }

    // Guard against a truncated buffer rather than indexing off the end.
    let need = a.width as usize * a.height as usize * 4;
    if a.rgba.len() < need || b.rgba.len() < need {
        return 1.0;
    }

    let blocks_x = a.width.div_ceil(BLOCK_PX) as usize;
    let blocks_y = a.height.div_ceil(BLOCK_PX) as usize;

    // One accumulator row at a time: sum of |delta| over R, G and B, plus the pixel count
    // so edge blocks that are clipped by the buffer bounds still average correctly.
    let mut sums   = vec![0u64; blocks_x];
    let mut counts = vec![0u32; blocks_x];
    let mut changed = 0usize;

    for by in 0..blocks_y {
        sums.iter_mut().for_each(|s| *s = 0);
        counts.iter_mut().for_each(|c| *c = 0);

        let y0 = by as u32 * BLOCK_PX;
        let y1 = (y0 + BLOCK_PX).min(a.height);

        for y in y0..y1 {
            let row = y as usize * a.width as usize * 4;

            for x in 0..a.width as usize {
                let i  = row + x * 4;
                let bx = x / BLOCK_PX as usize;

                // Alpha is ignored: shm capture buffers are opaque and an xrgb source has
                // no meaningful alpha to compare.
                let d = a.rgba[i    ].abs_diff(b.rgba[i    ]) as u64
                      + a.rgba[i + 1].abs_diff(b.rgba[i + 1]) as u64
                      + a.rgba[i + 2].abs_diff(b.rgba[i + 2]) as u64;

                sums[bx]   += d;
                counts[bx] += 1;
            }
        }

        for bx in 0..blocks_x {
            if counts[bx] == 0 {
                continue;
            }

            // Mean per-channel delta: three channels per pixel.
            let mean = sums[bx] / (counts[bx] as u64 * 3);
            if mean >= BLOCK_THRESHOLD as u64 {
                changed += 1;
            }
        }
    }

    changed as f32 / (blocks_x * blocks_y) as f32
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a frame of `w` by `h` filled with a solid colour.
    fn solid(w: u32, h: u32, rgb: [u8; 3]) -> Frame {
        let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);

        for _ in 0..w as usize * h as usize {
            rgba.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
        }

        Frame {
            output  : "TEST-1".to_string(),
            logical : Rect { x: 0.0, y: 0.0, w: f64::from(w), h: f64::from(h) },
            width   : w,
            height  : h,
            rgba    : rgba,
            t_s     : 0.0,
        }
    }

    /// Paints an axis-aligned rectangle of `rgb` into `f`.
    fn paint(f: &mut Frame, x0: u32, y0: u32, x1: u32, y1: u32, rgb: [u8; 3]) {
        for y in y0..y1 {
            for x in x0..x1 {
                let i = (y as usize * f.width as usize + x as usize) * 4;
                f.rgba[i    ] = rgb[0];
                f.rgba[i + 1] = rgb[1];
                f.rgba[i + 2] = rgb[2];
            }
        }
    }

    #[test]
    fn identical_frames_score_zero() {
        let a = solid(64, 32, [10, 20, 30]);
        let b = a.clone();

        assert_eq!(changed_fraction(&a, &b), 0.0);
    }

    #[test]
    fn differing_sizes_score_one() {
        let a = solid(64, 32, [0, 0, 0]);
        let b = solid(32, 32, [0, 0, 0]);

        assert_eq!(changed_fraction(&a, &b), 1.0);
    }

    #[test]
    fn full_repaint_scores_one() {
        let a = solid(64, 32, [0, 0, 0]);
        let b = solid(64, 32, [255, 255, 255]);

        assert_eq!(changed_fraction(&a, &b), 1.0);
    }

    #[test]
    fn quarter_repaint_scores_a_quarter() {
        // 64x32 is 8x4 = 32 blocks. Painting the top-left 32x16 covers 4x2 = 8 of them.
        let a = solid(64, 32, [0, 0, 0]);
        let mut b = a.clone();
        paint(&mut b, 0, 0, 32, 16, [255, 255, 255]);

        assert_eq!(changed_fraction(&a, &b), 0.25);
    }

    #[test]
    fn single_pixel_change_trips_exactly_one_block() {
        // One saturated pixel in a 64-pixel block averages 255/64 = 3.98 per channel,
        // just under the threshold, so bump two pixels to clear it.
        let a = solid(64, 32, [0, 0, 0]);
        let mut b = a.clone();
        paint(&mut b, 3, 3, 5, 4, [255, 255, 255]);

        let f = changed_fraction(&a, &b);
        assert!((f - 1.0 / 32.0).abs() < 1e-6, "expected one block of 32, got {f}");
    }

    #[test]
    fn noise_below_threshold_is_ignored() {
        let a = solid(64, 32, [128, 128, 128]);
        let b = solid(64, 32, [130, 130, 130]);

        assert_eq!(changed_fraction(&a, &b), 0.0);
    }

    #[test]
    fn partial_edge_blocks_are_counted() {
        // 20x12 leaves a 4-wide and a 4-tall remainder: 3x2 = 6 blocks total.
        let a = solid(20, 12, [0, 0, 0]);
        let mut b = a.clone();
        paint(&mut b, 16, 8, 20, 12, [255, 255, 255]);

        let f = changed_fraction(&a, &b);
        assert!((f - 1.0 / 6.0).abs() < 1e-6, "expected one block of 6, got {f}");
    }

    #[test]
    fn alpha_only_change_is_ignored() {
        let a = solid(16, 16, [50, 60, 70]);
        let mut b = a.clone();

        for i in (3..b.rgba.len()).step_by(4) {
            b.rgba[i] = 0;
        }

        assert_eq!(changed_fraction(&a, &b), 0.0);
    }

    /// Builds an output description with the given logical rectangle and physical mode.
    fn output(logical: (f64, f64, f64, f64), mode: (u32, u32)) -> OutputInfo {
        OutputInfo {
            name       : "TEST-1".to_string(),
            logical    : Rect { x: logical.0, y: logical.1, w: logical.2, h: logical.3 },
            scale      : f64::from(mode.0) / logical.2,
            physical_w : mode.0,
            physical_h : mode.1,
        }
    }

    #[test]
    fn scale_one_output_maps_by_pure_offset() {
        // DP-1: 3840x1600 at (2559, 0), unscaled.
        let p = output((2559.0, 0.0, 3840.0, 1600.0), (3840, 1600)).buffer_to_global(1356.0, 644.0);

        assert_eq!(p.x, 3915.0);
        assert_eq!(p.y, 644.0);
    }

    #[test]
    fn fractional_scale_divides_before_offsetting() {
        // HDMI-A-1: mode 1920x1200, logical 1670x1043 at (1506, 1600), scale ~1.1497.
        // The bottom-right physical pixel must land on the bottom-right logical pixel.
        let info = output((1506.0, 1600.0, 1670.0, 1043.0), (1920, 1200));
        let p    = info.buffer_to_global(1920.0, 1200.0);

        assert!((p.x - 3176.0).abs() < 1e-9, "x was {}", p.x);
        assert!((p.y - 2643.0).abs() < 1e-9, "y was {}", p.y);
    }

    #[test]
    fn buffer_origin_maps_to_the_logical_origin() {
        // DP-2: 2560x1440 at (0, 160), unscaled.
        let p = output((0.0, 160.0, 2560.0, 1440.0), (2560, 1440)).buffer_to_global(0.0, 0.0);

        assert_eq!(p.x, 0.0);
        assert_eq!(p.y, 160.0);
    }

    #[test]
    fn scale_follows_logical_width() {
        let mut f = solid(4, 4, [0, 0, 0]);
        f.width   = 1920;
        f.height  = 1200;
        f.logical = Rect { x: 1506.0, y: 1600.0, w: 960.0, h: 600.0 };

        assert_eq!(f.scale(), 2.0);
    }
}
