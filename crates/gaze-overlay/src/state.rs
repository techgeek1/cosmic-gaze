//! What the overlay is currently showing.

use gaze_core::{GlobalPx, Rect};

/// Everything the overlay draws, in global logical pixels.
///
/// Every field is optional and `None` means "draw nothing for this". The whole struct is
/// replaced on each update rather than mutated field by field, so a producer that stops
/// sending a highlight makes it disappear without needing a separate clear call.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OverlayState {
    /// Where the filtered gaze point is. Drawn as a ring with a centre dot.
    pub gaze      : Option<GlobalPx>,
    /// The candidate element the snap engine currently favours. Drawn as a stroked box
    /// with a faint interior.
    pub highlight : Option<Rect>,
    /// Debug only: where the gaze point really is, before provider noise. Drawn as a
    /// cross in a colour that cannot be confused with the gaze ring.
    pub truth     : Option<GlobalPx>,
    /// Debug only: a short ASCII caption drawn next to the highlight. Rendered with the
    /// built in 5x7 bitmap font, so anything outside printable ASCII becomes `?`.
    pub label     : Option<String>,
    /// A solid colour painted over the whole of every surface, behind everything else,
    /// as straight-alpha RGBA. `None` is the transparent debug overlay: the desktop
    /// shows through. Anything else hides the desktop, which is what a recording
    /// session wants when it needs the pupil at a known illumination.
    pub background : Option<[u8; 4]>,
}

// --- OverlayState ---

impl OverlayState {
    /// True when there is nothing at all to draw. The event loop uses this to skip work,
    /// not to skip the commit: clearing the last frame still needs one.
    pub fn is_blank(&self) -> bool {
        self.gaze.is_none()
            && self.highlight.is_none()
            && self.truth.is_none()
            && self.label.is_none()
            && self.background.is_none()
    }
}
