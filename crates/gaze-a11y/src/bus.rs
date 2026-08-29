//! The AT-SPI point query over D-Bus.
//!
//! Every call here is one D-Bus round trip on the session's accessibility bus, made
//! through `zbus`'s blocking API. There is no tree walk: a query is `GetAccessibleAtPoint`
//! on the window's frame, then role, name and extents of what came back, then a bounded
//! climb through `Parent` to the nearest ancestor that is something a person clicks.
//!
//! # Coordinates
//!
//! AT-SPI has two coordinate types, screen (0) and window (1), and on Wayland neither is
//! the desk, nor even reliably the window. Firefox reports its frame at `(20, 20)` in
//! *both* types, because its coordinate space is its surface including the 20 px
//! client-side shadow margin, and every node in the tree is offset by the same amount;
//! cosmic-comp's toplevel geometry excludes the shadow. Measured on the desk before this
//! was understood: every YouTube button sat 20 px below its pixels. Chromium's "screen"
//! numbers look like desk coordinates instead.
//!
//! The toolkit-agnostic rule is to make everything **frame-relative**: read the frame
//! node's own extents in the coordinate type being used, and treat them as the origin.
//! A desk point becomes `p - toplevel.origin + frame.origin`; a node's extents become
//! `extents - frame.origin + toplevel.origin`. Whatever space the toolkit answers in,
//! the frame is at the toplevel's rectangle, so the offset cancels. [`A11y::at`] tries
//! window coordinates first and screen coordinates second, and accepts an answer only
//! when the node's extents, asked the same way, contain the query point.
//!
//! Toolkits put the shadow in different places. Firefox's frame is `(20, 20) 1271x1428`
//! for a 1271x1428 toplevel: the origin carries the shadow. Chromium's (Discord) is
//! `(0, 0) 1291x1448` for the same toplevel: the *size* carries it, and every node was
//! 10 px right of and below its pixels until this was measured. The shadow is symmetric
//! in both, so the content origin is `frame.origin + (frame.size - toplevel.size) / 2`,
//! which is what [`FrameOrigin::of`] computes; a frame no larger than its toplevel adds
//! nothing.

use std::time::Instant;

use gaze_capture::Toplevel;
use gaze_core::{GlobalPx, Rect};
use tracing::debug;
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::OwnedObjectPath;

/// Interface names, spelled once.
const IFACE_ACCESSIBLE : &str = "org.a11y.atspi.Accessible";
const IFACE_COMPONENT  : &str = "org.a11y.atspi.Component";

/// The registry's root object, whose children are the applications.
const REGISTRY_BUS  : &str = "org.a11y.atspi.Registry";
const ROOT_PATH     : &str = "/org/a11y/atspi/accessible/root";

/// The path every implementation returns for "nothing here".
const NULL_PATH: &str = "/org/a11y/atspi/null";

/// How many parents [`A11y::at`] will visit looking for an actionable ancestor. Web
/// content nests deeply, but a click target is rarely more than a few levels above the
/// text or image it was made on.
const MAX_CLIMB: usize = 12;

/// AT-SPI coordinate types, as `Component` methods take them. Both are used
/// frame-relative; see the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoordMode {
    /// Coordinate type 1.
    Window,
    /// Coordinate type 0.
    Screen,
}

impl CoordMode {
    /// The `coord_type` argument.
    fn atspi(self) -> u32 {
        match self {
            CoordMode::Window => 1,
            CoordMode::Screen => 0,
        }
    }
}

/// The frame node's origin in the toolkit's coordinate space: what the toolkit thinks
/// is at the toplevel's top-left corner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FrameOrigin {
    x : i32,
    y : i32,
}

impl FrameOrigin {
    /// Where `window`'s top-left corner is in the space of a frame with `extents`.
    ///
    /// Any size the frame has beyond the toplevel is a client-side shadow, taken to be
    /// symmetric and added to the origin; a frame that is not larger contributes only
    /// its own origin.
    fn of(extents: (i32, i32, i32, i32), window: &Toplevel) -> Self {
        let (fx, fy, fw, fh) = extents;
        let inset_x          = (fw - window.rect.w.round() as i32).max(0) / 2;
        let inset_y          = (fh - window.rect.h.round() as i32).max(0) / 2;

        FrameOrigin {
            x : fx + inset_x,
            y : fy + inset_y,
        }
    }

    /// The point to send for desk point `p` in `window`.
    fn query_point(self, p: GlobalPx, window: &Toplevel) -> (i32, i32) {
        (
            (p.x - window.rect.x).round() as i32 + self.x,
            (p.y - window.rect.y).round() as i32 + self.y,
        )
    }

    /// Extents in the toolkit's space brought back to the desk.
    fn to_global(self, extents: (i32, i32, i32, i32), window: &Toplevel) -> Rect {
        let (x, y, w, h) = extents;

        Rect {
            x : window.rect.x + f64::from(x - self.x),
            y : window.rect.y + f64::from(y - self.y),
            w : f64::from(w),
            h : f64::from(h),
        }
    }
}

/// One accessible object.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    /// Unique bus name of the application that owns it.
    pub bus  : String,
    pub path : String,
    /// `GetRoleName`: `push button`, `link`, `entry`, `image`, `section`, ...
    pub role : String,
    pub name : String,
    /// Extents in global logical pixels, when the object has a `Component`.
    pub rect : Option<Rect>,
}

/// What the tree said is at a point.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    /// The deepest object at the point.
    pub leaf    : Node,
    /// The nearest ancestor-or-self with an actionable role (see [`is_actionable`]),
    /// which is what a click there was aimed at. `None` when the climb ran out.
    pub target  : Option<Node>,
    /// How many parents were visited to reach `target`.
    pub climbed : usize,
    /// The coordinate interpretation the window answered in.
    pub coord   : CoordMode,
}

/// An application on the bus.
#[derive(Clone, Debug, PartialEq)]
pub struct Application {
    pub bus     : String,
    pub name    : String,
    /// Names of its top-level frames, in child order.
    pub windows : Vec<String>,
}

/// Errors from the bus.
#[derive(Debug, thiserror::Error)]
pub enum A11yError {
    #[error("no accessibility bus: {detail}")]
    NoBus { detail: String },

    #[error("D-Bus call {method} failed: {detail}")]
    Call { method: &'static str, detail: String },
}

/// A connection to the session's accessibility bus.
pub struct A11y {
    conn : Connection,
}

// --- A11y ---

impl A11y {
    /// Asks the session bus where the accessibility bus is and connects to it.
    pub fn connect() -> Result<Self, A11yError> {
        let session = Connection::session()
            .map_err(|e| A11yError::NoBus { detail: format!("session bus: {e}") })?;

        let launcher = Proxy::new(&session, "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus")
            .map_err(|e| A11yError::NoBus { detail: e.to_string() })?;

        let address: String = launcher
            .call("GetAddress", &())
            .map_err(|e| A11yError::NoBus { detail: format!("org.a11y.Bus.GetAddress: {e}") })?;

        let conn = zbus::blocking::connection::Builder::address(address.as_str())
            .and_then(|b| b.build())
            .map_err(|e| A11yError::NoBus { detail: format!("{address}: {e}") })?;

        Ok(Self { conn: conn })
    }

    /// The applications on the bus, with their frame names.
    pub fn applications(&self) -> Result<Vec<Application>, A11yError> {
        let mut out = Vec::new();

        for (bus, path) in self.children(REGISTRY_BUS, ROOT_PATH)? {
            let name    = self.name(&bus, &path).unwrap_or_default();
            let windows = self
                .children(&bus, &path)
                .unwrap_or_default()
                .into_iter()
                .map(|(b, p)| self.name(&b, &p).unwrap_or_default())
                .collect();

            out.push(Application {
                bus     : bus,
                name    : name,
                windows : windows,
            });
        }

        Ok(out)
    }

    /// What is at desk point `p`, which the caller has already placed in `window`.
    ///
    /// `Ok(None)` when no application on the bus matches the window, or the window
    /// answers null in every coordinate interpretation: the application is not
    /// accessible, and the pixels are all there is.
    pub fn at(&mut self, p: GlobalPx, window: &Toplevel) -> Result<Option<Hit>, A11yError> {
        let started = Instant::now();

        let Some((bus, frame)) = self.frame_for(window)? else {
            debug!(app_id = %window.app_id, title = %window.title, "no accessible application for the window");

            return Ok(None);
        };

        for mode in [CoordMode::Window, CoordMode::Screen] {
            // The frame's own origin is the offset between the toolkit's space and the
            // toplevel; a frame without extents cannot be asked about points.
            let Some(frame_extents) = self.extents(&bus, &frame, mode.atspi()) else {
                continue;
            };

            let origin = FrameOrigin::of(frame_extents, window);
            let (x, y) = origin.query_point(p, window);
            let (leaf_bus, leaf_path) = self.at_point(&bus, &frame, x, y, mode.atspi())?;

            if leaf_path == NULL_PATH {
                continue;
            }

            // Accept the interpretation only if the node agrees it is there.
            let extents = self.extents(&leaf_bus, &leaf_path, mode.atspi());

            if let Some((ex, ey, ew, eh)) = extents
                && !(x >= ex && x < ex + ew && y >= ey && y < ey + eh)
            {
                debug!(?mode, x, y, ?extents, "node does not contain the query point");

                continue;
            }

            let leaf = self.node(&leaf_bus, &leaf_path, mode, origin, window)?;
            let (target, climbed) = self.climb(&leaf, mode, origin, window)?;

            debug!(?mode, role = %leaf.role, ms = started.elapsed().as_secs_f64() * 1000.0, "a11y hit");

            return Ok(Some(Hit {
                leaf    : leaf,
                target  : target,
                climbed : climbed,
                coord   : mode,
            }));
        }

        Ok(None)
    }

    // --- Lookups ---

    /// The application frame that is `window`, as (bus, path).
    ///
    /// Applications are matched by name against the app id, case-insensitively and
    /// loosely (`firefox` ~ `Firefox`, `com.discordapp.Discord` ~ `Discord`); frames by
    /// title, exactly then by prefix; and failing that the application's first frame.
    fn frame_for(&self, window: &Toplevel) -> Result<Option<(String, String)>, A11yError> {
        let app_key = window.app_id.rsplit('.').next().unwrap_or(&window.app_id).to_lowercase();

        for (bus, path) in self.children(REGISTRY_BUS, ROOT_PATH)? {
            let name = self.name(&bus, &path).unwrap_or_default().to_lowercase();

            if name.is_empty() || !(name.contains(&app_key) || app_key.contains(&name)) {
                continue;
            }

            let frames = self.children(&bus, &path).unwrap_or_default();
            let titled: Vec<(String, String, String)> = frames
                .into_iter()
                .map(|(b, p)| {
                    let title = self.name(&b, &p).unwrap_or_default();

                    (b, p, title)
                })
                .collect();

            let exact  = titled.iter().find(|(_, _, t)| *t == window.title);
            let prefix = titled.iter().find(|(_, _, t)| {
                !t.is_empty() && (window.title.starts_with(t.as_str()) || t.starts_with(&window.title))
            });

            if let Some((b, p, _)) = exact.or(prefix).or(titled.first()) {
                return Ok(Some((b.clone(), p.clone())));
            }
        }

        Ok(None)
    }

    /// `Component.GetAccessibleAtPoint`.
    fn at_point(&self, bus: &str, path: &str, x: i32, y: i32, coord: u32)
        -> Result<(String, String), A11yError>
    {
        let proxy = self.proxy(bus, path, IFACE_COMPONENT)?;
        let (b, p): (String, OwnedObjectPath) = proxy
            .call("GetAccessibleAtPoint", &(x, y, coord))
            .map_err(|e| A11yError::Call { method: "GetAccessibleAtPoint", detail: e.to_string() })?;

        Ok((b, p.to_string()))
    }

    /// `Component.GetExtents`, or `None` for an object without a `Component`.
    fn extents(&self, bus: &str, path: &str, coord: u32) -> Option<(i32, i32, i32, i32)> {
        let proxy = self.proxy(bus, path, IFACE_COMPONENT).ok()?;

        proxy.call("GetExtents", &(coord,)).ok()
    }

    /// The object's role, name and desk rectangle.
    fn node(&self, bus: &str, path: &str, mode: CoordMode, origin: FrameOrigin, window: &Toplevel)
        -> Result<Node, A11yError>
    {
        let proxy = self.proxy(bus, path, IFACE_ACCESSIBLE)?;
        let role: String = proxy
            .call("GetRoleName", &())
            .map_err(|e| A11yError::Call { method: "GetRoleName", detail: e.to_string() })?;

        Ok(Node {
            bus  : bus.to_string(),
            path : path.to_string(),
            role : role,
            name : self.name(bus, path).unwrap_or_default(),
            rect : self.extents(bus, path, mode.atspi()).map(|e| origin.to_global(e, window)),
        })
    }

    /// Climbs from `leaf` to the nearest actionable ancestor-or-self.
    fn climb(&self, leaf: &Node, mode: CoordMode, origin: FrameOrigin, window: &Toplevel)
        -> Result<(Option<Node>, usize), A11yError>
    {
        if is_actionable(&leaf.role) {
            return Ok((Some(leaf.clone()), 0));
        }

        let mut bus     = leaf.bus.clone();
        let mut path    = leaf.path.clone();
        let mut climbed = 0;

        while climbed < MAX_CLIMB {
            climbed += 1;

            let proxy = self.proxy(&bus, &path, IFACE_ACCESSIBLE)?;
            let (pb, pp): (String, OwnedObjectPath) = proxy
                .get_property("Parent")
                .map_err(|e| A11yError::Call { method: "Parent", detail: e.to_string() })?;

            let pp = pp.to_string();

            if pp == NULL_PATH || pp == ROOT_PATH || pp == path {
                break;
            }

            bus  = pb;
            path = pp;

            let node = self.node(&bus, &path, mode, origin, window)?;

            if is_actionable(&node.role) {
                return Ok((Some(node), climbed));
            }

            // Past the document there is only the frame and the application.
            if node.role == "frame" || node.role == "application" {
                break;
            }
        }

        Ok((None, climbed))
    }

    /// `Accessible.GetChildren`.
    fn children(&self, bus: &str, path: &str) -> Result<Vec<(String, String)>, A11yError> {
        let proxy = self.proxy(bus, path, IFACE_ACCESSIBLE)?;
        let kids: Vec<(String, OwnedObjectPath)> = proxy
            .call("GetChildren", &())
            .map_err(|e| A11yError::Call { method: "GetChildren", detail: e.to_string() })?;

        Ok(kids.into_iter().map(|(b, p)| (b, p.to_string())).collect())
    }

    /// The `Name` property.
    fn name(&self, bus: &str, path: &str) -> Option<String> {
        self.proxy(bus, path, IFACE_ACCESSIBLE).ok()?.get_property("Name").ok()
    }

    /// A proxy for one object.
    fn proxy(&self, bus: &str, path: &str, iface: &'static str) -> Result<Proxy<'_>, A11yError> {
        Proxy::new(&self.conn, bus.to_string(), path.to_string(), iface)
            .map_err(|e| A11yError::Call { method: "proxy", detail: e.to_string() })
    }
}

/// Whether a role is something a person clicks on purpose, as `GetRoleName` spells it.
///
/// `image` is here deliberately: a thumbnail is a click target in its own right, and a
/// collector wants to know the click was on a picture rather than on text.
pub fn is_actionable(role: &str) -> bool {
    matches!(
        role,
        "push button" | "button" | "toggle button" | "check box" | "radio button" | "link"
            | "entry" | "text" | "password text" | "combo box" | "list item" | "menu item"
            | "check menu item" | "radio menu item" | "page tab" | "slider" | "spin button"
            | "image" | "tree item" | "table cell" | "heading"
    )
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// A window at (1000, 200).
    fn window() -> Toplevel {
        Toplevel {
            title      : "t".into(),
            app_id     : "firefox".into(),
            rect       : Rect { x: 1000.0, y: 200.0, w: 800.0, h: 600.0 },
            output     : "DP-1".into(),
            activated  : true,
            minimized  : false,
            fullscreen : false,
            focus_rank : 1,
        }
    }

    #[test]
    fn the_frame_origin_cancels_whatever_space_the_toolkit_uses() {
        let p = GlobalPx { x: 1100.5, y: 250.0 };

        // Firefox: the frame sits at (20, 20) of its shadowed surface.
        let shadowed = FrameOrigin { x: 20, y: 20 };

        assert_eq!(shadowed.query_point(p, &window()), (121, 70));
        assert_eq!(shadowed.to_global((120, 70, 10, 20), &window()),
                   Rect { x: 1100.0, y: 250.0, w: 10.0, h: 20.0 });

        // Chromium: the frame reports something like desk coordinates.
        let global = FrameOrigin { x: 1000, y: 200 };

        assert_eq!(global.query_point(p, &window()), (1101, 250));
        assert_eq!(global.to_global((1100, 250, 10, 20), &window()),
                   Rect { x: 1100.0, y: 250.0, w: 10.0, h: 20.0 });

        // A toolkit whose window space starts at the frame.
        let plain = FrameOrigin { x: 0, y: 0 };

        assert_eq!(plain.query_point(p, &window()), (101, 50));
    }

    #[test]
    fn a_frame_larger_than_its_toplevel_hides_the_shadow_in_its_size() {
        let w = window();
        let (tw, th) = (w.rect.w as i32, w.rect.h as i32);

        // Firefox: origin carries the shadow, size matches the toplevel.
        assert_eq!(FrameOrigin::of((20, 20, tw, th), &w), FrameOrigin { x: 20, y: 20 });

        // Chromium: origin is zero, the frame is 20 px larger each way.
        assert_eq!(FrameOrigin::of((0, 0, tw + 20, th + 20), &w), FrameOrigin { x: 10, y: 10 });

        // A frame smaller than its toplevel (a toolkit rounding down) adds nothing.
        assert_eq!(FrameOrigin::of((0, 0, tw - 1, th), &w), FrameOrigin { x: 0, y: 0 });
    }

    #[test]
    fn actionable_roles() {
        for role in ["push button", "link", "entry", "image", "page tab"] {
            assert!(is_actionable(role), "{role}");
        }

        for role in ["section", "panel", "frame", "document web", "paragraph", ""] {
            assert!(!is_actionable(role), "{role}");
        }
    }
}
