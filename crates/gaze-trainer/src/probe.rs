//! The labelling wrapper: a container that knows what it holds.
//!
//! Every control the trainer shows is wrapped in a [`Probe`], which does two things a
//! plain container does not. On every draw it records its box in the shared
//! [`Registry`], which is how the planner knows where the controls currently are. And
//! on a mouse press over its box it publishes a message carrying its label, its box,
//! the pointer and the wall-clock time, synchronously in the event pass, before any
//! widget has reacted to the press. That message is what goes to the collector.
//!
//! Probes are never nested: a row and its checkbox are siblings, so exactly one probe
//! speaks for each press. Overlays (menus, dropdowns) are processed before the base
//! tree, so when a popup item and the widget beneath it both see a press, the popup's
//! message arrives first, and the application keeps the first message per press.

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use cosmic::iced::advanced::layout::{self, Layout};
use cosmic::iced::advanced::widget::{Operation, Tree, Widget};
use cosmic::iced::advanced::{Clipboard, Shell, mouse, overlay, renderer};
use cosmic::iced::widget::Container;
use cosmic::iced::{Event, Length, Point, Rectangle, Size, Vector};
use cosmic::{Element, Renderer, Theme};

use crate::scene::WidgetId;

/// What a probe says about the control it wraps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Label {
    pub id   : WidgetId,
    /// `gaze_core::ElementKind` in lowercase.
    pub kind : &'static str,
    /// The visible text, when there is one.
    pub text : Option<String>,
}

/// One control as last drawn.
#[derive(Clone, Debug)]
pub struct Entry {
    pub label  : Label,
    /// Window-local logical pixels, clipped to the viewport it was drawn in.
    pub bounds : Rectangle,
    /// The draw generation this entry belongs to.
    frame        : u64,
}

/// Where the probes report to.
#[derive(Debug, Default)]
pub struct Registry {
    /// Bumped by the application on every `view`, so entries from a layout that is no
    /// longer shown can be told from current ones.
    frame     : u64,
    /// The latest generation any probe has drawn in.
    drawn   : u64,
    entries : Vec<Entry>,
}

/// Shared handle to the registry.
pub type Shared = Arc<Mutex<Registry>>;

/// A press as a probe saw it.
#[derive(Clone, Debug)]
pub struct Press {
    pub label    : Label,
    pub bounds   : Rectangle,
    pub px       : Point,
    pub button   : mouse::Button,
    /// Wall-clock time of the event pass, seconds.
    pub t_unix_s : f64,
}

// --- Registry ---

impl Registry {
    /// Starts a new layout generation. Called from `view`.
    pub fn next_frame(&mut self) -> u64 {
        self.frame += 1;

        self.frame
    }

    /// Records a control drawn in generation `frame`.
    fn record(&mut self, frame: u64, label: Label, bounds: Rectangle) {
        if frame > self.drawn {
            self.drawn = frame;

            // A new frame: everything older is no longer on screen.
            self.entries.retain(|e| e.frame >= frame);
        }

        if frame < self.drawn {
            return;
        }

        match self.entries.iter_mut().find(|e| e.label.id == label.id) {
            Some(entry) => {
                entry.bounds = bounds;
                entry.frame    = frame;
            }
            None => self.entries.push(Entry { label: label, bounds: bounds, frame: frame }),
        }
    }

    /// The generation the current `view` is building.
    pub fn current(&self) -> u64 {
        self.frame
    }

    /// Whether the layout generation `frame` has been drawn at least once.
    pub fn has_drawn(&self, frame: u64) -> bool {
        self.drawn >= frame
    }

    /// Every control on screen as of the last draw.
    pub fn visible(&self) -> Vec<Entry> {
        self.entries.iter().filter(|e| e.frame == self.drawn).cloned().collect()
    }
}

// --- Probe ---

/// A container that labels its content. See the module docs.
pub struct Probe<'a, Message> {
    registry  : Shared,
    frame       : u64,
    label     : Label,
    container : Container<'a, Message, Theme, Renderer>,
    on_press  : Box<dyn Fn(Press) -> Message + 'a>,
}

impl<'a, Message> Probe<'a, Message> {
    /// Wraps `content`. `frame` is the application's current layout generation.
    pub fn new(
        registry : &Shared,
        frame      : u64,
        label    : Label,
        content  : impl Into<Element<'a, Message>>,
        on_press : impl Fn(Press) -> Message + 'a,
    )
        -> Self
    {
        Probe {
            registry  : Arc::clone(registry),
            frame       : frame,
            label     : label,
            container : Container::new(content),
            on_press  : Box::new(on_press),
        }
    }

    /// Width of the wrapper. Shrink by default, which is the content's own width.
    pub fn width(mut self, width: impl Into<Length>) -> Self {
        self.container = self.container.width(width);

        self
    }
}

impl<'a, Message> Widget<Message, Theme, Renderer> for Probe<'a, Message> {
    fn children(&self) -> Vec<Tree> {
        self.container.children()
    }

    fn state(&self) -> cosmic::iced::advanced::widget::tree::State {
        self.container.state()
    }

    fn tag(&self) -> cosmic::iced::advanced::widget::tree::Tag {
        self.container.tag()
    }

    fn diff(&mut self, tree: &mut Tree) {
        self.container.diff(tree);
    }

    fn size(&self) -> Size<Length> {
        self.container.size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.container.size_hint()
    }

    fn layout(&mut self, tree: &mut Tree, renderer: &Renderer, limits: &layout::Limits)
        -> layout::Node
    {
        self.container.layout(tree, renderer, limits)
    }

    fn operate(
        &mut self,
        tree      : &mut Tree,
        layout    : Layout<'_>,
        renderer  : &Renderer,
        operation : &mut dyn Operation,
    ) {
        self.container.operate(tree, layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree      : &mut Tree,
        event     : &Event,
        layout    : Layout<'_>,
        cursor    : mouse::Cursor,
        renderer  : &Renderer,
        clipboard : &mut dyn Clipboard,
        shell     : &mut Shell<'_, Message>,
        viewport  : &Rectangle,
    ) {
        if let Event::Mouse(mouse::Event::ButtonPressed(button)) = event {
            let bounds = layout.bounds();

            if let Some(px) = cursor.position_over(bounds) {
                shell.publish((self.on_press)(Press {
                    label    : self.label.clone(),
                    bounds   : bounds,
                    px       : px,
                    button   : *button,
                    t_unix_s : now_unix_s(),
                }));
            }
        }

        self.container.update(tree, event, layout, cursor, renderer, clipboard, shell, viewport);
    }

    fn mouse_interaction(
        &self,
        tree     : &Tree,
        layout   : Layout<'_>,
        cursor   : mouse::Cursor,
        viewport : &Rectangle,
        renderer : &Renderer,
    )
        -> mouse::Interaction
    {
        self.container.mouse_interaction(tree, layout, cursor, viewport, renderer)
    }

    fn draw(
        &self,
        tree     : &Tree,
        renderer : &mut Renderer,
        theme    : &Theme,
        style    : &renderer::Style,
        layout   : Layout<'_>,
        cursor   : mouse::Cursor,
        viewport : &Rectangle,
    ) {
        // Only what is actually on screen counts: a list row scrolled out of view
        // still gets a draw call, with a box outside the viewport.
        if let Some(shown) = layout.bounds().intersection(viewport)
            && let Ok(mut registry) = self.registry.lock()
        {
            registry.record(self.frame, self.label.clone(), shown);
        }

        self.container.draw(tree, renderer, theme, style, layout, cursor, viewport);
    }

    fn overlay<'b>(
        &'b mut self,
        tree        : &'b mut Tree,
        layout      : Layout<'b>,
        renderer    : &Renderer,
        viewport    : &Rectangle,
        translation : Vector,
    )
        -> Option<overlay::Element<'b, Message, Theme, Renderer>>
    {
        self.container.overlay(tree, layout, renderer, viewport, translation)
    }

    fn drag_destinations(
        &self,
        state          : &Tree,
        layout         : Layout<'_>,
        renderer       : &Renderer,
        dnd_rectangles : &mut cosmic::iced::advanced::clipboard::DndDestinationRectangles,
    ) {
        self.container.drag_destinations(state, layout, renderer, dnd_rectangles);
    }
}

impl<'a, Message: 'a> From<Probe<'a, Message>> for Element<'a, Message> {
    fn from(probe: Probe<'a, Message>) -> Self {
        Element::new(probe)
    }
}

/// Current unix time, seconds.
pub fn now_unix_s() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    fn label(id: WidgetId) -> Label {
        Label { id: id, kind: "button", text: None }
    }

    fn rect(x: f32, y: f32) -> Rectangle {
        Rectangle { x: x, y: y, width: 10.0, height: 10.0 }
    }

    #[test]
    fn a_new_generation_drops_what_the_old_one_drew() {
        let mut registry = Registry::default();

        let g1 = registry.next_frame();
        registry.record(g1, label(WidgetId::Nav(0)), rect(0.0, 0.0));
        registry.record(g1, label(WidgetId::Nav(1)), rect(0.0, 20.0));

        assert_eq!(registry.visible().len(), 2);
        assert!(registry.has_drawn(g1));

        let g2 = registry.next_frame();

        assert!(!registry.has_drawn(g2));

        registry.record(g2, label(WidgetId::Nav(1)), rect(5.0, 20.0));

        let visible = registry.visible();

        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].label.id, WidgetId::Nav(1));
        assert_eq!(visible[0].bounds.x, 5.0);
    }

    #[test]
    fn a_late_draw_from_an_old_generation_is_ignored() {
        let mut registry = Registry::default();

        let g1 = registry.next_frame();
        let g2 = registry.next_frame();

        registry.record(g2, label(WidgetId::Tab(0)), rect(0.0, 0.0));
        registry.record(g1, label(WidgetId::Tab(1)), rect(0.0, 0.0));

        assert_eq!(registry.visible().len(), 1);
    }
}
