//! Debug overlays: the screenshot with every trial target outlined in the colour of what
//! usually happened to it.
//!
//! This is the only part of the bench meant for eyes rather than for the report. Green
//! boxes are targets the engine reliably found, red ones are targets it reliably lost to
//! a neighbour, and a wall of red in one corner of one app says something quite different
//! from red scattered evenly.

use std::path::Path;

use anyhow::{Context, Result};
use gaze_core::Rect;
use image::{Rgba, RgbaImage};

use crate::run::{OverlayState, dominant_state};
use crate::shots::Shot;
use crate::stats::FrameResult;

/// Outline thickness in frame pixels. Two is visible on a 4K frame without swallowing the
/// small boxes it is drawn around.
const STROKE_PX : i64 = 2;

/// Colour of a target the engine mostly found and was confident about.
const CORRECT : Rgba<u8> = Rgba([64, 220, 96, 255]);

/// Colour of a target the engine mostly, confidently got wrong. The expensive failure.
const SLIPPED : Rgba<u8> = Rgba([235, 64, 64, 255]);

/// Colour of a target that mostly resolved with a rival too close to call, which the
/// two-tier design would hand to refinement rather than click.
const AMBIGUOUS : Rgba<u8> = Rgba([240, 176, 48, 255]);

/// Colour of a target the engine mostly declined to resolve.
const NONE : Rgba<u8> = Rgba([150, 150, 150, 255]);

/// Colour of a target whose gaze samples were mostly lost outright.
const NO_GAZE : Rgba<u8> = Rgba([200, 80, 220, 255]);

// --- Overlays ---

/// Writes `<output>-<n>-snap.png` into `dir`, the screenshot with one outline per element.
///
/// `frame` must be the result for `shot`; the element ids in it index `shot.elements`.
pub fn write_overlay(shot: &Shot, frame: &FrameResult, dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

    let mut image = image::open(&shot.path)
        .with_context(|| format!("reading {}", shot.path.display()))?
        .to_rgba8();

    // Passing boxes are drawn first so a failing box on top of a passing one stays
    // visible; the failures are what the overlay exists to show.
    let order = [
        OverlayState::ConfidentCorrect,
        OverlayState::NoTarget,
        OverlayState::NoGaze,
        OverlayState::Ambiguous,
        OverlayState::ConfidentWrong,
    ];

    for pass in order {
        for stats in &frame.elements {
            if dominant_state(&stats.tally) != pass {
                continue;
            }

            let Some(element) = shot.elements.get(stats.index) else {
                continue;
            };

            draw_rect(&mut image, shot, &element.bbox, colour(pass));
        }
    }

    let out = dir.join(format!("{}-{}-snap.png", shot.output, shot.index));

    image.save(&out).with_context(|| format!("writing {}", out.display()))?;

    Ok(())
}

/// Colour for an overlay state.
fn colour(state: OverlayState) -> Rgba<u8> {
    match state {
        OverlayState::ConfidentCorrect => CORRECT,
        OverlayState::ConfidentWrong   => SLIPPED,
        OverlayState::Ambiguous        => AMBIGUOUS,
        OverlayState::NoTarget         => NONE,
        OverlayState::NoGaze           => NO_GAZE,
    }
}

/// Draws a `STROKE_PX` outline of a global-logical rect onto a frame in physical pixels.
fn draw_rect(image: &mut RgbaImage, shot: &Shot, bbox: &Rect, colour: Rgba<u8>) {
    let x0 = ((bbox.x - shot.origin.x) * shot.scale).round() as i64;
    let y0 = ((bbox.y - shot.origin.y) * shot.scale).round() as i64;
    let x1 = x0 + (bbox.w * shot.scale).round() as i64;
    let y1 = y0 + (bbox.h * shot.scale).round() as i64;

    for t in 0..STROKE_PX {
        h_line(image, x0 - t, x1 + t, y0 - t, colour);
        h_line(image, x0 - t, x1 + t, y1 + t, colour);
        v_line(image, y0 - t, y1 + t, x0 - t, colour);
        v_line(image, y0 - t, y1 + t, x1 + t, colour);
    }
}

/// Horizontal run, clipped to the frame.
fn h_line(image: &mut RgbaImage, x0: i64, x1: i64, y: i64, colour: Rgba<u8>) {
    for x in x0..=x1 {
        put(image, x, y, colour);
    }
}

/// Vertical run, clipped to the frame.
fn v_line(image: &mut RgbaImage, y0: i64, y1: i64, x: i64, colour: Rgba<u8>) {
    for y in y0..=y1 {
        put(image, x, y, colour);
    }
}

/// Writes one pixel, ignoring coordinates outside the frame. Detector boxes can sit a
/// pixel or two off the edge after the scale conversion.
fn put(image: &mut RgbaImage, x: i64, y: i64, colour: Rgba<u8>) {
    if x < 0 || y < 0 || x >= image.width() as i64 || y >= image.height() as i64 {
        return;
    }

    image.put_pixel(x as u32, y as u32, colour);
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    use gaze_core::GlobalPx;

    /// A shot whose frame is twice the logical size, to exercise the scale conversion.
    fn shot() -> Shot {
        Shot {
            output   : "TEST-1".to_string(),
            index    : 0,
            path     : Path::new("unused.png").to_path_buf(),
            width    : 200,
            height   : 200,
            origin   : GlobalPx { x: 100.0, y: 50.0 },
            scale    : 2.0,
            elements : Vec::new(),
            widgets  : Vec::new(),
            filtered : 0,
        }
    }

    #[test]
    fn boxes_land_where_the_origin_and_scale_say_they_should() {
        let mut image = RgbaImage::new(200, 200);
        let bbox      = Rect { x: 110.0, y: 60.0, w: 20.0, h: 10.0 };

        draw_rect(&mut image, &shot(), &bbox, CORRECT);

        // (110 - 100) * 2 = 20 across, (60 - 50) * 2 = 20 down.
        assert_eq!(*image.get_pixel(20, 20), CORRECT);
        assert_eq!(*image.get_pixel(60, 40), CORRECT);
        // Well inside the outline, untouched.
        assert_eq!(*image.get_pixel(40, 30), Rgba([0, 0, 0, 0]));
    }

    #[test]
    fn drawing_off_the_edge_is_clipped_not_a_panic() {
        let mut image = RgbaImage::new(20, 20);
        let bbox      = Rect { x: 0.0, y: 0.0, w: 1000.0, h: 1000.0 };

        draw_rect(&mut image, &shot(), &bbox, SLIPPED);

        // The box starts far left and above the frame; nothing should have been written,
        // and nothing should have panicked.
        assert_eq!(*image.get_pixel(0, 0), Rgba([0, 0, 0, 0]));
    }
}
