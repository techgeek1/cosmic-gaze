//! Point sampling of an RGBA frame, shared by both models' preprocessing.
//!
//! Both preprocessors walk a destination grid and pull from the frame, rather than pushing
//! from the frame into the destination, so crop, scale and letterbox pad collapse into one
//! pass with no intermediate buffer.

/// Reads one RGBA texel as normalised RGB, dropping alpha. Screen captures are opaque, so
/// there is nothing to unpremultiply.
#[inline]
pub fn texel(rgba: &[u8], w: u32, x: u32, y: u32) -> [f32; 3] {
    let i = ((y as usize * w as usize) + x as usize) * 4;

    [
        rgba[i] as f32 / 255.0,
        rgba[i + 1] as f32 / 255.0,
        rgba[i + 2] as f32 / 255.0,
    ]
}

/// Reads the pixel covering `(x, y)`, or `None` when it lies outside the frame.
///
/// Used on the unscaled path, where it is an exact copy rather than a filter.
#[inline]
pub fn nearest(rgba: &[u8], w: u32, h: u32, x: f64, y: f64) -> Option<[f32; 3]> {
    let ix = x.round();
    let iy = y.round();

    if ix < 0.0 || iy < 0.0 || ix >= w as f64 || iy >= h as f64 {
        return None;
    }

    Some(texel(rgba, w, ix as u32, iy as u32))
}

/// Bilinearly filters the frame at `(x, y)`, or `None` when the sample centre lies outside
/// the frame. Neighbours are clamped to the edge so the filter never reads out of bounds.
#[inline]
pub fn bilinear(rgba: &[u8], w: u32, h: u32, x: f64, y: f64) -> Option<[f32; 3]> {
    if x < -0.5 || y < -0.5 || x > w as f64 - 0.5 || y > h as f64 - 0.5 {
        return None;
    }

    let fx0 = x.floor();
    let fy0 = y.floor();
    let tx  = (x - fx0) as f32;
    let ty  = (y - fy0) as f32;

    let x0 = (fx0.max(0.0) as u32).min(w - 1);
    let y0 = (fy0.max(0.0) as u32).min(h - 1);
    let x1 = (x0 + 1).min(w - 1);
    let y1 = (y0 + 1).min(h - 1);

    let a = texel(rgba, w, x0, y0);
    let b = texel(rgba, w, x1, y0);
    let c = texel(rgba, w, x0, y1);
    let d = texel(rgba, w, x1, y1);

    let mut px = [0.0_f32; 3];

    for i in 0..3 {
        let top    = a[i] + (b[i] - a[i]) * tx;
        let bottom = c[i] + (d[i] - c[i]) * tx;

        px[i] = top + (bottom - top) * ty;
    }

    Some(px)
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::{bilinear, nearest, texel};

    /// A 2x2 RGBA frame with distinct red values.
    fn frame() -> Vec<u8> {
        vec![
            0, 0, 0, 255,       255, 0, 0, 255,
            0, 0, 0, 255,       255, 0, 0, 255,
        ]
    }

    /// Texel reads are row-major with a four byte stride.
    #[test]
    fn texel_reads_row_major() {
        let f = frame();

        assert_eq!(texel(&f, 2, 0, 0)[0], 0.0);
        assert_eq!(texel(&f, 2, 1, 0)[0], 1.0);
        assert_eq!(texel(&f, 2, 1, 1)[0], 1.0);
    }

    /// Sampling outside the frame reports absence rather than clamping, so the caller can
    /// decide between letterbox fill and edge extension.
    #[test]
    fn out_of_frame_samples_are_none() {
        let f = frame();

        assert!(nearest(&f, 2, 2, -1.0, 0.0).is_none());
        assert!(nearest(&f, 2, 2, 2.0, 0.0).is_none());
        assert!(bilinear(&f, 2, 2, -0.6, 0.0).is_none());
        assert!(bilinear(&f, 2, 2, 1.6, 0.0).is_none());
    }

    /// Halfway between a black and a red pixel is half red.
    #[test]
    fn bilinear_interpolates_between_neighbours() {
        let f = frame();
        let p = bilinear(&f, 2, 2, 0.5, 0.0).unwrap();

        assert!((p[0] - 0.5).abs() < 1e-6, "{p:?}");
    }
}
