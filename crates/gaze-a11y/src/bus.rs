//! The AT-SPI point query over D-Bus.
//!
//! Every call here is one D-Bus round trip on the session's accessibility bus, made
//! through `zbus`'s blocking API. There is no tree walk: a query is `GetAccessibleAtPoint`
//! on the window's frame, then role, name and extents of what came back, then a bounded
//! climb through `Parent` to the nearest ancestor that is something a person clicks.
//!
//! The one exception is a hit on a *vacant* node: a nameless, childless, generic
//! container. Toolkits hit-test topmost first, and an invisible overlay drawn over the
//! content (VS Code keeps a full-window one for drag regions) wins the toolkit's own
//! descent with a node that has nothing to say, while the populated document sits in a
//! covered sibling branch. [`A11y::at`] punches through: climb from the vacant node and
//! hit-test each ancestor's other point-containing children, topmost first, taking the
//! first non-vacant answer.
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

/// How many ancestors [`A11y::at`] will climb from a vacant hit looking for the covered
/// sibling branch. The overlay and the content are usually direct siblings.
const MAX_PUNCH: usize = 4;

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
    /// The union of this node's children's extents, sampled from the first and last
    /// few placed children, global logical pixels. Only [`A11y::ancestors`] fills it;
    /// a hit never needs it. `None` when it was not asked for or nothing is placed.
    pub span : Option<Rect>,
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

/// Why the tree had no node for a point. The distinction matters to a caller deciding
/// whether the pixels should stand in: an application off the bus says nothing about
/// what is on screen, while an accessible window answering null says there is nothing
/// there it knows of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Miss {
    /// No application on the bus matches the window: not accessible, pixels only.
    Unreachable,
    /// The window answered null in every coordinate interpretation.
    Nothing,
    /// A node came back in some interpretation but its own extents excluded the point
    /// in every one, so no answer was trusted.
    Outside,
}

/// What the tree said about a point: a node, or why there is none.
// A hit is a few strings and a miss is a byte; the value lives for one call.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum Answer {
    Hit(Hit),
    Miss(Miss),
}

/// What [`A11y::resolve`] found: the leaf with the interpretation that produced it, or
/// why there is none.
enum Resolved {
    Leaf(Node, CoordMode, FrameOrigin),
    Miss(Miss),
}

/// The scrollable region under a point: the nearest ancestor whose content overflows
/// it vertically. See [`A11y::scroll_surface`] and [`clip_surface`].
#[derive(Clone, Debug, PartialEq)]
pub struct Surface {
    /// The clipping node: what a wheel event over `viewport` scrolls.
    pub clip     : Node,
    /// Its extents in global logical pixels. The bands an edge scroller wants are
    /// measured against this, never against the window.
    pub viewport : Rect,
    /// The overflowing child's extents, which extend above and/or below the viewport.
    /// How far it pokes out is how much there is left to scroll each way.
    pub content  : Rect,
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
    /// accessible, and the pixels are all there is. [`A11y::ask`] says which.
    pub fn at(&mut self, p: GlobalPx, window: &Toplevel) -> Result<Option<Hit>, A11yError> {
        Ok(match self.ask(p, window)? {
            Answer::Hit(hit) => Some(hit),
            Answer::Miss(_)  => None,
        })
    }

    /// [`A11y::at`] with the reason when there is no node.
    pub fn ask(&mut self, p: GlobalPx, window: &Toplevel) -> Result<Answer, A11yError> {
        let started = Instant::now();

        let (leaf, mode, origin) = match self.resolve(p, window)? {
            Resolved::Leaf(leaf, mode, origin) => (leaf, mode, origin),
            Resolved::Miss(miss)               => return Ok(Answer::Miss(miss)),
        };

        let (target, climbed) = self.climb(&leaf, mode, origin, window)?;

        debug!(?mode, role = %leaf.role, ms = started.elapsed().as_secs_f64() * 1000.0, "a11y hit");

        Ok(Answer::Hit(Hit {
            leaf    : leaf,
            target  : target,
            climbed : climbed,
            coord   : mode,
        }))
    }

    /// The node under `p` and every ancestor above it, leaf first, up to and including
    /// the frame. Diagnostic: this is the walk `at` deliberately does not make, and it
    /// exists so the shape of a toolkit's tree (which ancestor is the scroll surface, what
    /// it is called, what its extents are against its children's) can be read off the
    /// desk instead of guessed. Empty when the tree has no answer.
    pub fn ancestors(&mut self, p: GlobalPx, window: &Toplevel) -> Result<Vec<Node>, A11yError> {
        let Resolved::Leaf(leaf, mode, origin) = self.resolve(p, window)? else {
            return Ok(Vec::new());
        };

        let mut chain = vec![leaf];

        while chain.len() <= MAX_CLIMB {
            let last = chain.last().expect("non-empty");

            if last.role == "frame" || last.role == "application" {
                break;
            }

            let Some((pb, pp)) = self.parent(&last.bus, &last.path)? else {
                break;
            };

            let mut parent = self.node(&pb, &pp, mode, origin, window)?;

            parent.span = self.span(&pb, &pp, mode, origin, window);

            chain.push(parent);
        }

        Ok(chain)
    }

    /// The scrollable region under `p`: [`clip_surface`] over [`A11y::ancestors`].
    /// `Ok(None)` when the tree has no answer or nothing above the point overflows.
    pub fn scroll_surface(&mut self, p: GlobalPx, window: &Toplevel)
        -> Result<Option<Surface>, A11yError>
    {
        let started = Instant::now();
        let chain   = self.ancestors(p, window)?;
        let surface = clip_surface(&chain);

        debug!(depth = chain.len(), found = surface.is_some(),
               ms = started.elapsed().as_secs_f64() * 1000.0, "scroll surface");

        Ok(surface)
    }

    /// The leaf under `p`, with the coordinate interpretation and frame origin that
    /// produced it. Shared by [`A11y::at`] and [`A11y::ancestors`].
    fn resolve(&mut self, p: GlobalPx, window: &Toplevel)
        -> Result<Resolved, A11yError>
    {
        let Some((bus, frame)) = self.frame_for(window)? else {
            debug!(app_id = %window.app_id, title = %window.title, "no accessible application for the window");

            return Ok(Resolved::Miss(Miss::Unreachable));
        };

        let mut outside = false;

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

            // An invisible overlay covering the content wins the toolkit's hit test
            // with a node that says nothing; ask what it was covering instead.
            let mut hit = (leaf_bus, leaf_path);

            if self.is_vacant(&hit.0, &hit.1)
                && let Some(covered) = self.punch_through(hit.clone(), &frame, x, y, mode.atspi())
            {
                debug!(?mode, from = %hit.1, to = %covered.1, "punched through a vacant hit");
                hit = covered;
            }

            let (leaf_bus, leaf_path) = hit;

            // Accept the interpretation only if the node agrees it is there.
            let extents = self.extents(&leaf_bus, &leaf_path, mode.atspi());

            if let Some(extents) = extents
                && !contains(extents, x, y)
            {
                debug!(?mode, x, y, ?extents, "node does not contain the query point");
                outside = true;

                continue;
            }

            let leaf = self.node(&leaf_bus, &leaf_path, mode, origin, window)?;

            return Ok(Resolved::Leaf(leaf, mode, origin));
        }

        Ok(Resolved::Miss(if outside { Miss::Outside } else { Miss::Nothing }))
    }

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
            span : None,
        })
    }

    /// The union of the first and last [`SPAN_SAMPLE`] placed children's extents. A
    /// scroll container whose rows are its direct children (YouTube's mix list) has no
    /// single overflowing child for the chain to find; its rows above and below the one
    /// under the point are where the overflow is, and its first and last children are
    /// the far ends of it. Sampling both ends costs a `GetChildren` and a few extents
    /// per ancestor instead of one per row.
    fn span(&self, bus: &str, path: &str, mode: CoordMode, origin: FrameOrigin, window: &Toplevel)
        -> Option<Rect>
    {
        let kids = self.children(bus, path).ok()?;
        let n    = kids.len();

        let head = kids.iter().take(SPAN_SAMPLE);
        let tail = kids.iter().skip(n.max(SPAN_SAMPLE) - SPAN_SAMPLE).skip_while(|_| n <= SPAN_SAMPLE);

        head.chain(tail)
            .filter_map(|(b, p)| self.extents(b, p, mode.atspi()))
            .map(|e| origin.to_global(e, window))
            .filter(|r| r.w > 0.0 && r.h > 0.0)
            .reduce(union)
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

            let Some((pb, pp)) = self.parent(&bus, &path)? else {
                break;
            };

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

    /// A replacement for a vacant hit: the covered sibling branch's answer.
    ///
    /// Climbs from the vacant node; at each ancestor, hit-tests the other children that
    /// contain the point, topmost (last) first, and returns the first non-vacant answer.
    /// Opportunistic: any failure along the way just means no better answer.
    fn punch_through(&self, vacant: (String, String), frame: &str, x: i32, y: i32, coord: u32)
        -> Option<(String, String)>
    {
        let mut came = vacant;

        for _ in 0..MAX_PUNCH {
            let (pb, pp) = self.parent(&came.0, &came.1).ok().flatten()?;

            for (cb, cp) in self.children(&pb, &pp).unwrap_or_default().into_iter().rev() {
                if cb == came.0 && cp == came.1 {
                    continue;
                }

                let Some(extents) = self.extents(&cb, &cp, coord) else { continue };

                if !contains(extents, x, y) {
                    continue;
                }

                // The branch's own hit test, or the branch itself if it answers null.
                let Ok((hb, hp)) = self.at_point(&cb, &cp, x, y, coord) else { continue };
                let (hb, hp)     = if hp == NULL_PATH { (cb, cp) } else { (hb, hp) };

                if !self.is_vacant(&hb, &hp) {
                    return Some((hb, hp));
                }
            }

            if pp == frame {
                return None;
            }

            came = (pb, pp);
        }

        None
    }

    /// Whether a node is a nameless, childless, generic container: a hit on one says
    /// nothing about what is drawn there.
    fn is_vacant(&self, bus: &str, path: &str) -> bool {
        let role: Option<String> = self
            .proxy(bus, path, IFACE_ACCESSIBLE)
            .ok()
            .and_then(|p| p.call("GetRoleName", &()).ok());

        if !matches!(role.as_deref(), Some("panel" | "filler" | "unknown" | "redundant object")) {
            return false;
        }

        if self.name(bus, path).is_some_and(|n| !n.is_empty()) {
            return false;
        }

        let count: Option<i32> = self
            .proxy(bus, path, IFACE_ACCESSIBLE)
            .ok()
            .and_then(|p| p.get_property("ChildCount").ok());

        count == Some(0)
    }

    /// The `Parent` property, or `None` past the top of the tree.
    fn parent(&self, bus: &str, path: &str) -> Result<Option<(String, String)>, A11yError> {
        let proxy = self.proxy(bus, path, IFACE_ACCESSIBLE)?;
        let (pb, pp): (String, OwnedObjectPath) = proxy
            .get_property("Parent")
            .map_err(|e| A11yError::Call { method: "Parent", detail: e.to_string() })?;

        let pp = pp.to_string();

        if pp == NULL_PATH || pp == ROOT_PATH || pp == path {
            return Ok(None);
        }

        Ok(Some((pb, pp)))
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
/// Slack in the overflow test, logical pixels: rounding between a toolkit's layout and
/// the integer extents it reports.
const OVERFLOW_SLACK_PX: f64 = 1.0;

/// How many children from each end of a node are measured for its span.
const SPAN_SAMPLE: usize = 2;

/// A clipping node shorter than this is not a scroll viewport, and the walk continues
/// past it. A table cell whose glyphs overhang it, a one-line label taller than its row:
/// these overflow their parent by the geometric test too, and taking the first of them
/// hides the page scroll behind a clip nothing can usefully scroll.
pub const MIN_CLIP_PX: f64 = 120.0;

/// The scroll surface in an ancestor chain (leaf first, as [`A11y::ancestors`] returns
/// it): the first ancestor whose child's extents extend above or below its own.
///
/// Toolkits do not label scroll surfaces. AT-SPI has a `scroll pane` role and GTK uses
/// it, but Firefox scrolls its page as a `document web`, and a Discord list or a
/// scrollable `div` anywhere is a plain `section` or `panel` with nothing in its role or
/// state set saying it clips. What every toolkit does report is geometry: the clipping
/// node's extents are its viewport, and the child it clips keeps its full laid-out
/// extents, which overflow the viewport. Measured on the desk 2026-09-04, this rule
/// found Discord's message list (a 4305 px `list` in an 898 x 1296 `panel` ending above
/// the composer), Discord's channel sidebar (a 2272 px `list` in a 302 x 1292
/// `section`) and Firefox's page (a 12901 px `landmark` in a 1269 x 1343
/// `document web`), each with the right viewport. The same test says "nothing to
/// scroll" for content that fits, which is also the right answer.
///
/// Horizontal overflow is ignored: a carousel is not what a vertical wheel moves. A clip
/// shorter than [`MIN_CLIP_PX`] is skipped for the next one up, see there.
///
/// A parent whose child on the chain fits is also tested against its sampled children
/// span ([`Node::span`]): a list whose rows are its direct children overflows by its
/// first and last rows, never by the row under the point. The content reported is then
/// the union of the chain child and the span.
///
/// Nodes without real extents are left out of the chain before pairing: Firefox reports
/// some structural nodes at `-1x-1` (a `section` between YouTube's page and its
/// document, 2026-09-09), and one of those taken as a child put "content" 106 px above
/// a document that was at the top of its page, so the upper band offered a scroll up
/// that did not exist and the real overflow below was never reached.
pub fn clip_surface(chain: &[Node]) -> Option<Surface> {
    let placed : Vec<&Node> = chain
        .iter()
        .filter(|n| n.rect.is_some_and(|r| r.w > 0.0 && r.h > 0.0))
        .collect();

    for pair in placed.windows(2) {
        let (Some(child), Some(parent)) = (pair[0].rect, pair[1].rect) else {
            continue;
        };

        if parent.h < MIN_CLIP_PX {
            continue;
        }

        let content = pair[1].span.map_or(child, |span| union(child, span));
        let above   = content.y < parent.y - OVERFLOW_SLACK_PX;
        let below   = content.y + content.h > parent.y + parent.h + OVERFLOW_SLACK_PX;

        if above || below {
            return Some(Surface {
                clip     : pair[1].clone(),
                viewport : parent,
                content  : content,
            });
        }
    }

    None
}

/// The smallest rectangle holding both.
fn union(a: Rect, b: Rect) -> Rect {
    let x0 = a.x.min(b.x);
    let y0 = a.y.min(b.y);
    let x1 = (a.x + a.w).max(b.x + b.w);
    let y1 = (a.y + a.h).max(b.y + b.h);

    Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
}

pub fn is_actionable(role: &str) -> bool {
    matches!(
        role,
        "push button" | "button" | "toggle button" | "check box" | "radio button" | "link"
            | "entry" | "text" | "password text" | "combo box" | "list item" | "menu item"
            | "check menu item" | "radio menu item" | "page tab" | "slider" | "spin button"
            | "image" | "tree item" | "table cell" | "heading"
    )
}

/// Whether extents `(x, y, w, h)` contain the point.
fn contains(extents: (i32, i32, i32, i32), x: i32, y: i32) -> bool {
    let (ex, ey, ew, eh) = extents;

    x >= ex && x < ex + ew && y >= ey && y < ey + eh
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

    /// A node with only what `clip_surface` reads.
    fn boxed(role: &str, x: f64, y: f64, w: f64, h: f64) -> Node {
        Node {
            bus  : ":1.4".to_string(),
            path : format!("/{role}"),
            role : role.to_string(),
            name : String::new(),
            rect : Some(Rect { x: x, y: y, w: w, h: h }),
            span : None,
        }
    }

    #[test]
    fn the_discord_message_list_clips_at_the_panel_above_the_composer() {
        // `gaze-a11y-cli chain 756,866`, 2026-09-04, leaf first.
        let chain = vec![
            boxed("image"    , 469.0,   802.0, 399.0,  225.0),
            boxed("button"   , 469.0,   802.0, 399.0,  225.0),
            boxed("article"  , 453.0,   655.0, 432.0,  417.0),
            boxed("section"  , 453.0,   653.0, 786.0,  421.0),
            boxed("list item", 381.0,   585.0, 882.0,  491.0),
            boxed("list"     , 381.0, -2761.0, 882.0, 4305.0),
            boxed("section"  , 381.0, -2761.0, 882.0, 4305.0),
            boxed("panel"    , 381.0,   248.0, 898.0, 1296.0),
            boxed("landmark" , 381.0,   248.0, 898.0, 1346.0),
        ];

        let surface = clip_surface(&chain).expect("the list overflows the panel");

        assert_eq!(surface.clip.role, "panel");
        assert_eq!(surface.viewport, Rect { x: 381.0, y: 248.0, w: 898.0, h: 1296.0 });
        assert_eq!(surface.content.h, 4305.0);
    }

    /// YouTube's mix list in Firefox: the rows are direct children of the 356 px clip, so
    /// the row under the point fits it and only the sampled span of its siblings (740 to
    /// 3024) says it scrolls. The taller containers above it hold their children.
    #[test]
    fn a_list_whose_rows_are_its_children_overflows_by_its_span() {
        // `gaze-a11y-cli chain 2300,1200 --window YouTube`, 2026-09-09, leaf first.
        let spanned = |role: &str, x: f64, y: f64, w: f64, h: f64, top: f64, bottom: f64| Node {
            span : Some(Rect { x: x, y: top, w: w, h: bottom - top }),
            ..boxed(role, x, y, w, h)
        };
        let chain = vec![
            boxed  ("section", 2213.0, 1156.0, 100.0,   56.0),
            spanned("section", 2213.0, 1156.0, 100.0,   56.0, 1156.0, 1212.0),
            spanned("section", 2189.0, 1156.0, 300.0,   62.0, 1156.0, 1218.0),
            spanned("link"   , 2189.0, 1156.0, 300.0,   62.0, 1156.0, 1218.0),
            spanned("section", 2189.0, 1152.0, 348.0,   70.0, 1156.0, 1218.0),
            spanned("section", 2189.0, 1149.0, 348.0,  356.0,  740.0, 3024.0),
            spanned("section", 2188.0, 1046.0, 350.0,  460.0, 1047.0, 1505.0),
            spanned("section", 2188.0, 1046.0, 350.0, 4031.0, 1522.0, 5077.0),
        ];

        let surface = clip_surface(&chain).expect("the rows overflow the list");

        assert_eq!(surface.viewport, Rect { x: 2189.0, y: 1149.0, w: 348.0, h: 356.0 });
        assert_eq!(surface.content.y, 740.0);
        assert_eq!(surface.content.y + surface.content.h, 3024.0);
    }

    /// YouTube in Firefox at the top of the page: a `-1x-1` section between the page and
    /// the document is not content, so the surface is the document with everything
    /// below and nothing above.
    #[test]
    fn a_node_without_extents_is_not_overflowing_content() {
        // `gaze-a11y-cli chain 1500,900 --window YouTube`, 2026-09-09, leaf first.
        let chain = vec![
            boxed("panel"       , 1283.0, 307.0, 1271.0,  715.0),
            boxed("section"     , 1283.0, 307.0, 1271.0,  715.0),
            boxed("landmark"    , 1283.0, 307.0, 1271.0, 4780.0),
            boxed("section"     , 1283.0, 307.0, 1271.0, 4780.0),
            boxed("section"     , 1283.0, 251.0, 1271.0, 4836.0),
            boxed("section"     , 1262.0, 145.0,   -1.0,   -1.0),
            boxed("document web", 1283.0, 251.0, 1271.0, 1343.0),
        ];

        let surface = clip_surface(&chain).expect("the page overflows the document");

        assert_eq!(surface.clip.role, "document web");
        assert_eq!(surface.content, Rect { x: 1283.0, y: 251.0, w: 1271.0, h: 4836.0 });
        assert!(surface.content.y >= surface.viewport.y, "nothing above: at the top of the page");
    }

    #[test]
    fn the_firefox_page_clips_at_the_document_not_the_window() {
        // `gaze-a11y-cli chain 1985,966`: five same-sized sections above the landmark
        // are all overflowing children of the document, and none of them is the clip.
        let chain = vec![
            boxed("section"     , 1350.0,   960.0,  827.0,    72.0),
            boxed("article"     , 1350.0, -4931.0,  827.0, 11676.0),
            boxed("section"     , 1318.0, -4963.0,  891.0, 11740.0),
            boxed("section"     , 1285.0, -6043.0, 1269.0, 12901.0),
            boxed("landmark"    , 1285.0, -6043.0, 1269.0, 12901.0),
            boxed("document web", 1285.0,   251.0, 1269.0,  1343.0),
            boxed("scroll pane" , 1285.0,   251.0, 1269.0,  1343.0),
            boxed("frame"       , 1285.0,   166.0, 1269.0,  1428.0),
        ];

        let surface = clip_surface(&chain).expect("the landmark overflows the document");

        assert_eq!(surface.clip.role, "document web");
        assert_eq!(surface.viewport.h, 1343.0);
    }

    #[test]
    fn content_that_fits_has_no_surface_and_a_carousel_does_not_count() {
        let fits = vec![
            boxed("button" , 100.0, 100.0,  50.0,  20.0),
            boxed("section",  90.0,  90.0, 200.0, 100.0),
            boxed("frame"  ,   0.0,   0.0, 800.0, 600.0),
        ];

        assert_eq!(clip_surface(&fits), None);

        // A row of cards wider than its strip: horizontal overflow only.
        let carousel = vec![
            boxed("image"  , 100.0, 100.0, 100.0,  80.0),
            boxed("list"   ,   0.0,  95.0, 3000.0, 90.0),
            boxed("section",  50.0,  95.0,  700.0, 90.0),
            boxed("frame"  ,   0.0,   0.0,  800.0, 600.0),
        ];

        assert_eq!(clip_surface(&carousel), None);
    }

    #[test]
    fn a_cell_its_glyphs_overhang_is_not_the_clip_the_page_is() {
        // A diff's line-number cell: the text node pokes out of the 20 px cell, which by
        // the raw test is a clip. The page scroll is three levels up.
        let chain = vec![
            boxed("text"        , 1300.0,   498.0,   40.0,    24.0),
            boxed("table cell"  , 1300.0,   500.0,   40.0,    20.0),
            boxed("table row"   , 1300.0,   500.0,  900.0,    20.0),
            boxed("table"       , 1300.0, -3000.0,  900.0,  9000.0),
            boxed("document web", 1285.0,   251.0, 1269.0,  1343.0),
            boxed("frame"       , 1285.0,   166.0, 1269.0,  1428.0),
        ];

        let surface = clip_surface(&chain).expect("the table overflows the document");

        assert_eq!(surface.clip.role, "document web");
        assert_eq!(surface.content.h, 9000.0);
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
