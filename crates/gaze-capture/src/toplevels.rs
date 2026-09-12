//! Where every window is, in global logical pixels.
//!
//! Wayland clients do not know where they are on screen, by design, so anything a client
//! reports about its own contents (an accessibility tree, say) is in window coordinates
//! and useless on its own. cosmic-comp fills the gap: `zcosmic_toplevel_info_v1` (version
//! 2 and up) sends every toplevel's position and size relative to each output it is on,
//! and `ext_foreign_toplevel_list_v1` names it. Joining the two with the outputs' logical
//! origins gives each window a rectangle on the same desk the pointer and the captures
//! use.
//!
//! Workspaces come from `ext_workspace_manager_v1`: which are active, and (through the
//! cosmic handle's `ext_workspace_enter`) which each window is on. A window on no active
//! workspace is not on screen however recently it was focused, and the compositor keeps
//! reporting its geometry, so [`ToplevelTracker::at`] leaves it out. A compositor
//! without the workspace global, or a handle that never named one, counts as visible.
//!
//! The protocol carries no stacking order, and it matters: two maximised windows on one
//! output have identical rectangles, and only one of them is on screen. What the
//! tracker does see is *activation over time*, and focus is a good proxy for the top of
//! the stack: the window most recently activated among those containing the point is
//! the one that was raised. [`ToplevelTracker::at`] uses that first, then the currently
//! activated window, then the smallest containing one (a dialog sits on its parent).
//! The first is only as good as the tracker's history, so a long-running tracker
//! answers better than a fresh one; a caller that can verify the answer against the
//! pixels should. The complete answer would be a capture of each candidate toplevel
//! (`ext_foreign_toplevel_image_capture_source_manager_v1`) compared with the screen.

use std::time::Duration;

use cosmic_protocols::toplevel_info::v1::client::zcosmic_toplevel_handle_v1::{
    Event as CosmicEvent, State, ZcosmicToplevelHandleV1,
};
use cosmic_protocols::toplevel_info::v1::client::zcosmic_toplevel_info_v1::{
    Event as InfoEvent, ZcosmicToplevelInfoV1,
};
use gaze_core::{GlobalPx, Rect};
use wayland_client::backend::ObjectId;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_output::{Event as WlOutputEvent, WlOutput};
use wayland_client::protocol::wl_registry::{Event as RegistryEvent, WlRegistry};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop, event_created_child,
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::{
    Event as HandleEvent, ExtForeignToplevelHandleV1,
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_list_v1::{
    EVT_TOPLEVEL_OPCODE, Event as ListEvent, ExtForeignToplevelListV1,
};
use wayland_protocols::ext::workspace::v1::client::ext_workspace_group_handle_v1::ExtWorkspaceGroupHandleV1;
use wayland_protocols::ext::workspace::v1::client::ext_workspace_handle_v1::{
    Event as WorkspaceEvent, ExtWorkspaceHandleV1, State as WorkspaceState,
};
use wayland_protocols::ext::workspace::v1::client::ext_workspace_manager_v1::{
    EVT_WORKSPACE_GROUP_OPCODE, EVT_WORKSPACE_OPCODE, Event as WorkspaceManagerEvent,
    ExtWorkspaceManagerV1,
};
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_manager_v1::ZxdgOutputManagerV1;
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_v1::{
    Event as XdgOutputEvent, ZxdgOutputV1,
};

use crate::capture::CaptureError;
use crate::outputs::{
    OutputEntry, XDG_OUTPUT_VERSION, apply_wl_output, apply_xdg_output, bind_output, is_output,
    release_output,
};

/// How long `connect` waits for the compositor to place every window it listed.
const CONNECT_BUDGET: Duration = Duration::from_millis(500);

/// `zcosmic_toplevel_info_v1` versions this module speaks: 2 brought `geometry` and the
/// `ext_foreign_toplevel_list_v1` pairing, 3 is what cosmic-comp advertises.
const TOPLEVEL_INFO_VERSIONS: std::ops::RangeInclusive<u32> = 2..=3;

/// One window as the compositor describes it.
#[derive(Clone, Debug, PartialEq)]
pub struct Toplevel {
    /// Window title, as the application set it.
    pub title      : String,
    /// Application id (`firefox`, `com.discordapp.Discord`).
    pub app_id     : String,
    /// The window's rectangle in global logical pixels, on `output`.
    pub rect       : Rect,
    /// Connector the rectangle is relative to. A window spanning two outputs reports
    /// one rectangle per output and appears once per output.
    pub output     : String,
    /// Has keyboard focus.
    pub activated  : bool,
    pub minimized  : bool,
    /// On an active workspace, so on screen as far as workspaces go. True when the
    /// compositor offers no workspace list or never placed this window on one.
    pub visible    : bool,
    pub fullscreen : bool,
    /// When this window was last activated, as a rank among the tracker's observations:
    /// higher is more recent, zero is never while the tracker was watching.
    pub focus_rank : u64,
}

/// Follows the compositor's toplevel list.
///
/// Not `Send`, like the other trackers: the event queue and its proxies belong to the
/// thread that built them.
pub struct ToplevelTracker {
    conn  : Connection,
    queue : EventQueue<ToplevelState>,
    state : ToplevelState,
}

// --- ToplevelTracker ---

impl ToplevelTracker {
    /// Connects to the compositor and receives the current toplevel list.
    pub fn connect() -> Result<Self, CaptureError> {
        let conn = Connection::connect_to_env()
            .map_err(|e| CaptureError::Connect { detail: e.to_string() })?;

        let (globals, mut queue) = registry_queue_init::<ToplevelState>(&conn)
            .map_err(|e| CaptureError::Connect { detail: e.to_string() })?;

        let qh = queue.handle();

        let xdg_mgr: ZxdgOutputManagerV1 = globals
            .bind(&qh, 1..=XDG_OUTPUT_VERSION, ())
            .map_err(|_| CaptureError::MissingGlobal { interface: "zxdg_output_manager_v1" })?;

        let info: ZcosmicToplevelInfoV1 = globals
            .bind(&qh, TOPLEVEL_INFO_VERSIONS, ())
            .map_err(|_| CaptureError::MissingGlobal {
                interface: "zcosmic_toplevel_info_v1 (version 2)",
            })?;

        // Optional: without it every window counts as visible.
        let workspaces: Option<ExtWorkspaceManagerV1> = globals.bind(&qh, 1..=1, ()).ok();

        let mut state = ToplevelState {
            xdg_mgr    : xdg_mgr,
            info       : info,
            _workspace : workspaces,
            outputs    : Vec::new(),
            toplevels  : Vec::new(),
            workspaces : Vec::new(),
            focus_seq  : 0,
        };

        let registry = globals.registry().clone();

        for global in globals.contents().clone_list() {
            if is_output(&global.interface) {
                let entry = bind_output(&qh, &registry, &state.xdg_mgr, global.name, global.version);
                state.outputs.push(entry);
            }
        }

        // The list must be bound after the outputs are known, or the first geometry
        // events would name outputs the tracker cannot resolve yet. The same round trip
        // brings the workspace handles, which the toplevels' workspace events name.
        queue
            .roundtrip(&mut state)
            .map_err(|e| CaptureError::Protocol { detail: e.to_string() })?;

        let _list: ExtForeignToplevelListV1 = globals
            .bind(&qh, 1..=1, ())
            .map_err(|_| CaptureError::MissingGlobal { interface: "ext_foreign_toplevel_list_v1" })?;

        // The handles arrive on the first round trip and their titles on the second.
        // The cosmic extension's geometry does not: cosmic-comp answers
        // `get_cosmic_toplevel` from its own loop, after the `sync` reply, so no number
        // of round trips brings it. Wait on the socket instead, until every window has
        // a rectangle or the budget is spent.
        for _ in 0..2 {
            queue
                .roundtrip(&mut state)
                .map_err(|e| CaptureError::Protocol { detail: e.to_string() })?;
        }

        let mut tracker = Self {
            conn  : conn,
            queue : queue,
            state : state,
        };

        let started = std::time::Instant::now();

        while started.elapsed() < CONNECT_BUDGET && !tracker.complete() {
            tracker.wait(Duration::from_millis(20))?;
        }

        Ok(tracker)
    }

    /// Whether every known window has at least one rectangle.
    fn complete(&self) -> bool {
        self.state.toplevels.iter().all(|t| !t.placements.is_empty())
    }

    /// Folds in whatever the compositor has sent since the last call, without waiting.
    pub fn pump(&mut self) -> Result<(), CaptureError> {
        self.conn
            .flush()
            .map_err(|e| CaptureError::Protocol { detail: e.to_string() })?;

        if let Some(guard) = self.queue.prepare_read() {
            // A would-block here means nothing arrived, which is not an error.
            let _ = guard.read();
        }

        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(|e| CaptureError::Protocol { detail: e.to_string() })?;

        Ok(())
    }

    /// Blocks for up to `timeout` for the compositor's next batch, then folds it in.
    /// Handy for a follower that wants to react to moves without polling.
    pub fn wait(&mut self, timeout: Duration) -> Result<(), CaptureError> {
        use rustix::event::{PollFd, PollFlags, poll};

        self.conn
            .flush()
            .map_err(|e| CaptureError::Protocol { detail: e.to_string() })?;

        let Some(guard) = self.queue.prepare_read() else {
            return self.pump();
        };

        let fd      = guard.connection_fd();
        let mut fds = [PollFd::new(&fd, PollFlags::IN)];
        let spec    = rustix::time::Timespec {
            tv_sec  : timeout.as_secs() as i64,
            tv_nsec : timeout.subsec_nanos() as _,
        };

        match poll(&mut fds, Some(&spec)) {
            Ok(_)                                    => {}
            Err(e) if e == rustix::io::Errno::INTR   => {}
            Err(e) => return Err(CaptureError::Protocol { detail: format!("poll: {e}") }),
        }

        let _ = guard.read();

        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(|e| CaptureError::Protocol { detail: e.to_string() })?;

        Ok(())
    }

    /// Every window with a known rectangle, one entry per output it is on. Call
    /// [`pump`](Self::pump) first for a current answer.
    pub fn toplevels(&self) -> Vec<Toplevel> {
        let mut out = Vec::new();

        for entry in &self.state.toplevels {
            for placement in &entry.placements {
                let Some(output) = self.state.outputs.iter().find(|o| o.global_name == placement.output) else {
                    continue;
                };
                let Some(info) = output.info() else {
                    continue;
                };

                out.push(Toplevel {
                    title      : entry.title.clone(),
                    app_id     : entry.app_id.clone(),
                    rect       : Rect {
                        x : info.logical.x + f64::from(placement.x),
                        y : info.logical.y + f64::from(placement.y),
                        w : f64::from(placement.w),
                        h : f64::from(placement.h),
                    },
                    output     : info.name,
                    activated  : entry.activated,
                    minimized  : entry.minimized,
                    visible    : self.state.visible(entry),
                    fullscreen : entry.fullscreen,
                    focus_rank : entry.focus_rank,
                });
            }
        }

        out
    }

    /// The window most likely on top at `p`: the most recently activated one containing
    /// the point, then the currently activated one, then the smallest containing one.
    /// Minimized windows and windows on no active workspace never match.
    pub fn at(&self, p: GlobalPx) -> Option<Toplevel> {
        let containing: Vec<Toplevel> = self
            .toplevels()
            .into_iter()
            .filter(|t| !t.minimized && t.visible && t.rect.contains(p))
            .collect();

        containing
            .iter()
            .max_by_key(|t| (t.focus_rank, t.activated, -((t.rect.w * t.rect.h) as i64)))
            .cloned()
    }
}

// --- ToplevelState ---

/// Everything the event handlers read and write.
struct ToplevelState {
    xdg_mgr    : ZxdgOutputManagerV1,
    info       : ZcosmicToplevelInfoV1,
    /// Held so the workspace events keep coming; `None` without the global.
    _workspace : Option<ExtWorkspaceManagerV1>,
    outputs    : Vec<OutputEntry>,
    toplevels  : Vec<ToplevelEntry>,
    /// Every workspace the compositor announced, with whether it is active.
    workspaces : Vec<WorkspaceEntry>,
    /// Activations seen so far; the next one gets this plus one.
    focus_seq  : u64,
}

/// One workspace and the one thing the tracker wants of it.
struct WorkspaceEntry {
    handle : ExtWorkspaceHandleV1,
    active : bool,
}

/// One toplevel's handles and the properties received so far.
struct ToplevelEntry {
    /// The `ext_foreign_toplevel_handle_v1`, identified by its object id.
    handle     : ExtForeignToplevelHandleV1,
    /// The cosmic extension object, requested as soon as the handle arrives.
    cosmic     : ZcosmicToplevelHandleV1,
    title      : String,
    app_id     : String,
    /// One rectangle per output the window is on, output-relative logical pixels.
    placements : Vec<Placement>,
    activated  : bool,
    minimized  : bool,
    fullscreen : bool,
    /// See [`Toplevel::focus_rank`].
    focus_rank : u64,
    /// Object ids of the workspaces this window is on. Empty until the compositor
    /// says, which counts as visible.
    workspaces : Vec<ObjectId>,
}

/// A window's rectangle relative to one output.
#[derive(Clone, Copy, Debug)]
struct Placement {
    /// Registry name of the output.
    output : u32,
    x      : i32,
    y      : i32,
    w      : i32,
    h      : i32,
}

impl ToplevelState {
    /// The entry whose cosmic handle is `id`.
    fn by_cosmic(&mut self, id: &ObjectId) -> Option<&mut ToplevelEntry> {
        self.toplevels.iter_mut().find(|t| t.cosmic.id() == *id)
    }

    /// The entry whose foreign handle is `id`.
    fn by_handle(&mut self, id: &ObjectId) -> Option<&mut ToplevelEntry> {
        self.toplevels.iter_mut().find(|t| t.handle.id() == *id)
    }

    /// Whether `entry` is on an active workspace, or on none the tracker knows of.
    fn visible(&self, entry: &ToplevelEntry) -> bool {
        if entry.workspaces.is_empty() {
            return true;
        }

        entry.workspaces.iter().any(|id| {
            self.workspaces.iter().any(|w| w.handle.id() == *id && w.active)
        })
    }

    /// Drops an entry and its proxies.
    fn close(&mut self, id: &ObjectId) {
        if let Some(pos) = self.toplevels.iter().position(|t| t.handle.id() == *id || t.cosmic.id() == *id) {
            let entry = self.toplevels.remove(pos);

            entry.cosmic.destroy();
            entry.handle.destroy();
        }
    }

    /// Registry name of the output behind a `wl_output` proxy.
    fn output_name(&self, output: &WlOutput) -> Option<u32> {
        self.outputs.iter().find(|o| o.wl_output.id() == output.id()).map(|o| o.global_name)
    }

    /// Forgets an output that went away, and the placements on it.
    fn remove_output(&mut self, global_name: u32) {
        let Some(pos) = self.outputs.iter().position(|o| o.global_name == global_name) else {
            return;
        };

        release_output(self.outputs.remove(pos));

        for entry in &mut self.toplevels {
            entry.placements.retain(|p| p.output != global_name);
        }
    }
}

// --- Dispatch ---

impl Dispatch<WlRegistry, GlobalListContents> for ToplevelState {
    /// Tracks outputs appearing and disappearing at runtime.
    fn event(
        state    : &mut Self,
        registry : &WlRegistry,
        event    : RegistryEvent,
        _data    : &GlobalListContents,
        _conn    : &Connection,
        qh       : &QueueHandle<Self>,
    ) {
        match event {
            RegistryEvent::Global { name, interface, version } if is_output(&interface) => {
                if state.outputs.iter().any(|o| o.global_name == name) {
                    return;
                }

                let entry = bind_output(qh, registry, &state.xdg_mgr, name, version);
                state.outputs.push(entry);
            }

            RegistryEvent::GlobalRemove { name } => {
                state.remove_output(name);
            }

            _ => {}
        }
    }
}

impl Dispatch<WlOutput, u32> for ToplevelState {
    fn event(
        state  : &mut Self,
        _proxy : &WlOutput,
        event  : WlOutputEvent,
        data   : &u32,
        _conn  : &Connection,
        _qh    : &QueueHandle<Self>,
    ) {
        if let Some(entry) = state.outputs.iter_mut().find(|o| o.global_name == *data) {
            apply_wl_output(entry, event);
        }
    }
}

impl Dispatch<ZxdgOutputV1, u32> for ToplevelState {
    fn event(
        state  : &mut Self,
        _proxy : &ZxdgOutputV1,
        event  : XdgOutputEvent,
        data   : &u32,
        _conn  : &Connection,
        _qh    : &QueueHandle<Self>,
    ) {
        if let Some(entry) = state.outputs.iter_mut().find(|o| o.global_name == *data) {
            apply_xdg_output(entry, event);
        }
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for ToplevelState {
    /// Opens an entry, and the cosmic extension object, for every new handle.
    fn event(
        state  : &mut Self,
        _proxy : &ExtForeignToplevelListV1,
        event  : ListEvent,
        _data  : &(),
        _conn  : &Connection,
        qh     : &QueueHandle<Self>,
    ) {
        if let ListEvent::Toplevel { toplevel } = event {
            let cosmic = state.info.get_cosmic_toplevel(&toplevel, qh, ());

            state.toplevels.push(ToplevelEntry {
                handle     : toplevel,
                cosmic     : cosmic,
                title      : String::new(),
                app_id     : String::new(),
                placements : Vec::new(),
                activated  : false,
                minimized  : false,
                fullscreen : false,
                focus_rank : 0,
                workspaces : Vec::new(),
            });
        }
    }

    event_created_child!(ToplevelState, ExtForeignToplevelListV1, [
        EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for ToplevelState {
    /// Title and app id; `closed` drops the entry.
    fn event(
        state : &mut Self,
        proxy : &ExtForeignToplevelHandleV1,
        event : HandleEvent,
        _data : &(),
        _conn : &Connection,
        _qh   : &QueueHandle<Self>,
    ) {
        let id = proxy.id();

        match event {
            HandleEvent::Title { title } => {
                if let Some(entry) = state.by_handle(&id) {
                    entry.title = title;
                }
            }

            HandleEvent::AppId { app_id } => {
                if let Some(entry) = state.by_handle(&id) {
                    entry.app_id = app_id;
                }
            }

            HandleEvent::Closed => {
                state.close(&id);
            }

            _ => {}
        }
    }
}

impl Dispatch<ZcosmicToplevelInfoV1, ()> for ToplevelState {
    fn event(
        _state : &mut Self,
        _proxy : &ZcosmicToplevelInfoV1,
        _event : InfoEvent,
        _data  : &(),
        _conn  : &Connection,
        _qh    : &QueueHandle<Self>,
    ) {
        // `done` is the only event a version 2 client receives, and nothing here needs
        // atomic batches: a caller reads the list after `pump`, never mid-dispatch.
    }

    // The deprecated version 1 `toplevel` event, never sent to a version 2 client. The
    // dispatcher still needs to know what it would create.
    event_created_child!(ToplevelState, ZcosmicToplevelInfoV1, [
        0 => (ZcosmicToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ZcosmicToplevelHandleV1, ()> for ToplevelState {
    /// Geometry and state, the two things the ext handle does not carry.
    fn event(
        state : &mut Self,
        proxy : &ZcosmicToplevelHandleV1,
        event : CosmicEvent,
        _data : &(),
        _conn : &Connection,
        _qh   : &QueueHandle<Self>,
    ) {
        let id = proxy.id();

        match event {
            CosmicEvent::Geometry { output, x, y, width, height } => {
                let Some(name) = state.output_name(&output) else {
                    return;
                };
                let Some(entry) = state.by_cosmic(&id) else {
                    return;
                };

                entry.placements.retain(|p| p.output != name);
                entry.placements.push(Placement {
                    output : name,
                    x      : x,
                    y      : y,
                    w      : width,
                    h      : height,
                });
            }

            CosmicEvent::OutputLeave { output } => {
                let Some(name) = state.output_name(&output) else {
                    return;
                };

                if let Some(entry) = state.by_cosmic(&id) {
                    entry.placements.retain(|p| p.output != name);
                }
            }

            CosmicEvent::State { state: raw } => {
                let states: Vec<u32> = raw
                    .chunks_exact(4)
                    .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                    .collect();
                let has = |s: State| states.contains(&(s as u32));

                let next    = state.focus_seq + 1;
                let mut raised = false;

                if let Some(entry) = state.by_cosmic(&id) {
                    let activated = has(State::Activated);

                    if activated && !entry.activated {
                        entry.focus_rank = next;
                        raised           = true;
                    }

                    entry.activated  = activated;
                    entry.minimized  = has(State::Minimized);
                    entry.fullscreen = has(State::Fullscreen);
                }

                if raised {
                    state.focus_seq = next;
                }
            }

            CosmicEvent::ExtWorkspaceEnter { workspace } => {
                if let Some(entry) = state.by_cosmic(&id)
                    && !entry.workspaces.contains(&workspace.id())
                {
                    entry.workspaces.push(workspace.id());
                }
            }

            CosmicEvent::ExtWorkspaceLeave { workspace } => {
                if let Some(entry) = state.by_cosmic(&id) {
                    entry.workspaces.retain(|w| *w != workspace.id());
                }
            }

            CosmicEvent::Closed => {
                state.close(&id);
            }

            _ => {}
        }
    }
}

impl Dispatch<ExtWorkspaceManagerV1, ()> for ToplevelState {
    /// New workspaces are kept; groups are of no interest but must be created.
    fn event(
        state : &mut Self,
        _proxy: &ExtWorkspaceManagerV1,
        event : WorkspaceManagerEvent,
        _data : &(),
        _conn : &Connection,
        _qh   : &QueueHandle<Self>,
    ) {
        if let WorkspaceManagerEvent::Workspace { workspace } = event {
            state.workspaces.push(WorkspaceEntry { handle: workspace, active: false });
        }
    }

    event_created_child!(ToplevelState, ExtWorkspaceManagerV1, [
        EVT_WORKSPACE_GROUP_OPCODE => (ExtWorkspaceGroupHandleV1, ()),
        EVT_WORKSPACE_OPCODE       => (ExtWorkspaceHandleV1, ()),
    ]);
}

impl Dispatch<ExtWorkspaceHandleV1, ()> for ToplevelState {
    /// Only the active bit matters; `removed` drops the workspace.
    fn event(
        state : &mut Self,
        proxy : &ExtWorkspaceHandleV1,
        event : WorkspaceEvent,
        _data : &(),
        _conn : &Connection,
        _qh   : &QueueHandle<Self>,
    ) {
        let id = proxy.id();

        match event {
            WorkspaceEvent::State { state: raw } => {
                let active = raw
                    .into_result()
                    .is_ok_and(|flags| flags.contains(WorkspaceState::Active));

                if let Some(w) = state.workspaces.iter_mut().find(|w| w.handle.id() == id) {
                    w.active = active;
                }
            }

            WorkspaceEvent::Removed => {
                state.workspaces.retain(|w| w.handle.id() != id);

                for entry in &mut state.toplevels {
                    entry.workspaces.retain(|w| *w != id);
                }

                proxy.destroy();
            }

            _ => {}
        }
    }
}

delegate_noop!(ToplevelState: ignore ExtWorkspaceGroupHandleV1);
delegate_noop!(ToplevelState: ignore ZxdgOutputManagerV1);
