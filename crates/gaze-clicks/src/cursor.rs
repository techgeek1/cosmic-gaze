//! The pointer's shape, read from the cursor image's hotspot and size.
//!
//! The application under the pointer chooses the cursor, and in doing so says what it
//! thinks is there: a hand over a link or a card, an I-beam over text or an input, an
//! arrow over nothing in particular. That is the one signal about the screen the
//! recogniser cannot get from the pixels, and it covers exactly the controls the widget
//! model misses: input fields drawn as a slightly different grey, clickable regions with
//! no border, and text in a terminal.
//!
//! The compositor never names the shape. What `ext_image_copy_capture_cursor_session_v1`
//! gives is the image's size and its hotspot, and those are enough: an arrow's hotspot
//! sits in the top-left corner, a hand's at the top edge under the fingertip, an
//! I-beam's dead centre. The bands in [`classify`] come from the 24 px images of the two
//! themes on the desk, Adwaita and Pop, whose hotspots are in the tests. The centre is
//! shared with `wait`, `crosshair` and every resize cursor, so an I-beam is only called
//! by exact match against the themes' `text` hotspots, and any other centred image is
//! [`CursorShape::Centred`].

use std::fmt;

/// A cursor image's hotspot and size, both in the image's own pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorImage {
    pub hotspot_x : i32,
    pub hotspot_y : i32,
    pub w         : u32,
    pub h         : u32,
}

impl CursorImage {
    /// `WxH@X,Y`, size then hotspot.
    pub fn describe(&self) -> String {
        format!("{}x{}@{},{}", self.w, self.h, self.hotspot_x, self.hotspot_y)
    }
}

/// What the pointer looked like.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorShape {
    /// The default arrow, or one of its variants (`progress`, `context-menu`, `copy`).
    Arrow,
    /// A hand: `pointer` over something clickable, or `grab`/`grabbing`.
    Hand,
    /// An I-beam over text or an input.
    Text,
    /// Hotspot in the middle of the image but not a known I-beam: `wait`, `crosshair`,
    /// a resize arrow, `not-allowed`.
    Centred,
    /// Anything else.
    Other,
}

impl CursorShape {
    /// The name written to the session file.
    pub fn name(self) -> &'static str {
        match self {
            CursorShape::Arrow   => "arrow",
            CursorShape::Hand    => "hand",
            CursorShape::Text    => "text",
            CursorShape::Centred => "centred",
            CursorShape::Other   => "other",
        }
    }

    /// Whether the application put a hand or an I-beam up: its own word that something
    /// is under the pointer, whatever the recogniser found.
    pub fn says_something_is_there(self) -> bool {
        matches!(self, CursorShape::Hand | CursorShape::Text)
    }
}

impl fmt::Display for CursorShape {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

// --- Classification ---

/// Hotspots of the `text` and `vertical-text` images, as (size, x, y), for the themes
/// on the desk. Both ship the same 24 px hotspots: Adwaita `text` and Pop `text`,
/// `ibeam` and `xterm` are all `@11,12`; the vertical I-beams are `@12,11`. Larger
/// sizes scale these; add them here when a desk runs one.
const TEXT_HOTSPOTS: &[(u32, i32, i32)] = &[
    (24, 11, 12),
    (24, 12, 11),
];

/// The shape of a cursor image.
///
/// Bands are on the hotspot's position as a fraction of the image, so the same rule
/// holds across sizes. From the 24 px themes: arrows sit at `@3,1` and `@4,4`, hands at
/// `@7,5`, `@8,5`, `@9,5`, `@11,2` and `@11,7`, and everything with a centred hotspot
/// (`@11,11` to `@12,13`) is a wait, a crosshair, a resize or an I-beam, which only the
/// exact list tells apart.
pub fn classify(image: CursorImage) -> CursorShape {
    if image.w == 0 || image.h == 0 {
        return CursorShape::Other;
    }

    if TEXT_HOTSPOTS.contains(&(image.w, image.hotspot_x, image.hotspot_y)) && image.h == image.w {
        return CursorShape::Text;
    }

    let nx = (image.hotspot_x as f64 + 0.5) / image.w as f64;
    let ny = (image.hotspot_y as f64 + 0.5) / image.h as f64;

    if nx < 0.25 && ny < 0.25 {
        return CursorShape::Arrow;
    }

    if ny < 0.35 && (0.2..=0.6).contains(&nx) {
        return CursorShape::Hand;
    }

    if (0.4..=0.6).contains(&nx) && (0.4..=0.6).contains(&ny) {
        return CursorShape::Centred;
    }

    CursorShape::Other
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// A 24 px image with its hotspot at `(x, y)`.
    fn at(x: i32, y: i32) -> CursorImage {
        CursorImage {
            hotspot_x : x,
            hotspot_y : y,
            w         : 24,
            h         : 24,
        }
    }

    #[test]
    fn arrows_from_both_themes() {
        // Adwaita `default`, Pop `default`, Pop `progress`.
        for (x, y) in [(3, 1), (4, 4), (4, 3)] {
            assert_eq!(classify(at(x, y)), CursorShape::Arrow, "@{x},{y}");
        }
    }

    #[test]
    fn hands_from_both_themes() {
        // Adwaita `pointer`, `grab`, `grabbing`; Pop `pointer`, `grab`.
        for (x, y) in [(7, 5), (11, 2), (9, 5), (8, 5), (11, 7)] {
            assert_eq!(classify(at(x, y)), CursorShape::Hand, "@{x},{y}");
        }
    }

    #[test]
    fn i_beams_only_by_exact_hotspot() {
        assert_eq!(classify(at(11, 12)), CursorShape::Text);
        assert_eq!(classify(at(12, 11)), CursorShape::Text);

        // `wait` and `crosshair` in both themes, `not-allowed`, `ns-resize`.
        for (x, y) in [(11, 11), (12, 12), (12, 13)] {
            assert_eq!(classify(at(x, y)), CursorShape::Centred, "@{x},{y}");
        }
    }

    #[test]
    fn odd_hotspots_are_other() {
        // Adwaita `help` (bottom edge), `e-resize` (right edge), `alias`.
        for (x, y) in [(12, 21), (19, 13), (18, 5)] {
            assert_eq!(classify(at(x, y)), CursorShape::Other, "@{x},{y}");
        }

        assert_eq!(classify(CursorImage { hotspot_x: 0, hotspot_y: 0, w: 0, h: 0 }),
                   CursorShape::Other);
    }

    #[test]
    fn hands_and_i_beams_say_something_is_there() {
        assert!(CursorShape::Hand.says_something_is_there());
        assert!(CursorShape::Text.says_something_is_there());
        assert!(!CursorShape::Arrow.says_something_is_there());
        assert!(!CursorShape::Centred.says_something_is_there());
    }
}
