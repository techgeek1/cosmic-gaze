//! gaze-trainer: a working application whose every control is labelled.
//!
//! Passive clicks (`gaze-clicks`) are accurate labels but slow to accumulate and
//! biased toward wherever the user's own applications put their controls. A dot
//! ceremony is fast but the eye does not treat a dot on a blank field the way it
//! treats a button it is about to press. This is the third thing: a real libcosmic
//! application with menus, a sidebar, tabs, lists, forms, grids and dialogs, in six
//! archetypes (settings, files, mail, editor, browser, store) so the layouts differ,
//! which the user simply navigates. There is no task and no target: the clicks are
//! whatever the user finds worth clicking, which is what makes them ordinary. It knows
//! its own widget boxes, so it tells the collector what was under each press over a
//! socket (`gaze_core::trainer`), and the collector writes the click into the same
//! session format as everything else with `source = "trainer"`.
//!
//! Coverage steering: a histogram over the screen, seeded from every click already in
//! the session files, biases where each new window puts its sidebar and its toolbar,
//! so the emptier half of the screen gets the controls. Posture prompts every so many
//! labelled presses vary the head; the theme flips between dark and light per window
//! to vary the pupil.
//!
//! - [`scene`]: the generated application, its archetypes and how it reacts.
//! - [`probe`]: the wrapper that labels a control and reports its press.
//! - [`coverage`]: the histogram and the layout bias it asks for.
//! - [`link`]: the socket to the collector.
//! - [`words`]: the vocabulary the content is generated from.
//! - [`app`]: the libcosmic application tying them together.

// The workspace style mandates explicit `Foo { x: x }` field syntax everywhere.
#![allow(clippy::redundant_field_names)]

pub mod app;
pub mod coverage;
pub mod link;
pub mod probe;
pub mod scene;
pub mod words;

pub use app::{App, Config};
