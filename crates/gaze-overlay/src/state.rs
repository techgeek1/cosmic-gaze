//! What the overlay is currently showing.

use gaze_core::{GlobalPx, Rect};

/// Everything the overlay draws, in global logical pixels.
///
/// Every field is optional and `None` means "draw nothing for this". The whole struct is
/// replaced on each update rather than mutated field by field, so a producer that stops
/// sending a highlight makes it disappear without needing a separate clear call.
///
/// Two looks share this struct. `gaze`, `highlight`, `truth` and `label` are the debug
/// look: drawn exactly as given, on the frame they arrive, in fixed colours. `pointer`
/// is the daily-driver look: an intent the overlay presents on its own clock, fading
/// the highlight in and out, trailing the dot through a saccade and colouring both from
/// the desktop theme ([`crate::Presenter`]). A producer normally sets one or the other.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OverlayState {
    /// Where the filtered gaze point is. Drawn as a ring with a centre dot.
    pub gaze       : Option<GlobalPx>,
    /// The candidate element the snap engine currently favours. Drawn as a stroked box
    /// with a faint interior.
    pub highlight  : Option<Rect>,
    /// Debug only: where the gaze point really is, before provider noise. Drawn as a
    /// cross in a colour that cannot be confused with the gaze ring.
    pub truth      : Option<GlobalPx>,
    /// Debug only: a short ASCII caption drawn next to the highlight. Rendered with the
    /// built in 5x7 bitmap font, so anything outside printable ASCII becomes `?`.
    pub label      : Option<String>,
    /// A solid colour painted over the whole of every surface, behind everything else,
    /// as straight-alpha RGBA. `None` is the transparent debug overlay: the desktop
    /// shows through. Anything else hides the desktop, which is what a recording
    /// session wants when it needs the pupil at a known illumination.
    pub background : Option<[u8; 4]>,
    /// The pointer look. `None` takes it down (the dot and any highlight fade out).
    pub pointer    : Option<Pointer>,
}

/// The pointer look as the producer wants it: what is where, and whether the eyes are
/// moving. How that is shown, the fades, the trail and the colours, is the overlay's.
///
/// The dot is drawn only while `near` is set, so the point of gaze stays unmarked over
/// the text the user is reading and appears as the eyes reach something that can be
/// clicked. The highlight is drawn for `target` and follows it from element to element
/// with a crossfade.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pointer {
    /// Where the dot goes: the filtered gaze point, or the refined commit point while
    /// the fine channel is moving it.
    pub gaze   : GlobalPx,
    /// Whether the eyes are in flight, have just landed, or have settled. Decides the
    /// trail and the dot's weight.
    pub motion : Motion,
    /// An interactive element is within snapping reach. Shows the dot.
    pub near   : bool,
    /// The element the snap engine currently favours, when it is one worth marking.
    pub target : Option<Target>,
}

/// The element the highlight marks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Target {
    /// The producer's element id. The highlight crossfades when this changes, and only
    /// moves when it does not (the same element reported at a new place).
    pub id   : u64,
    /// The element's box in global logical pixels.
    pub rect : Rect,
}

/// What the eyes are doing, as far as the pointer look cares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    /// A saccade, or the filter has not yet decided. The dot is at full weight and
    /// trails.
    Moving,
    /// A fixation that began a moment ago. Full weight, no trail: the dot has arrived.
    Settling,
    /// A fixation that has held. On a target the dot fades to a ghost, since the
    /// highlight already says where a commit will land and the dot is on the fovea.
    Settled,
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
            && self.pointer.is_none()
    }
}
