//! Pointer position feedback over `ext_image_copy_capture_cursor_session_v1`.
//!
//! cosmic-comp maps an absolute uinput device onto a single output, so injection has to be
//! relative and closed loop: move, measure where the pointer actually went, correct. This
//! is the measurement half. It is deliberately a separate `wl_display` connection from
//! [`Capture`](crate::Capture) so a slow frame copy cannot delay a position reading, and
//! so a caller can track the pointer without ever allocating a capture buffer.
//!
//! One cursor session is opened per output, because the protocol reports the pointer
//! relative to a single capture source. Sessions are created and destroyed as outputs come
//! and go, the same way the frame path re-enumerates on every capture.
//!
//! ## Units
//!
//! `position` arrives in the capture source's **buffer pixels**, which are physical, not
//! logical. cosmic-comp 1.6.0 computes it as the pointer's output-local logical position
//! multiplied by the output's fractional scale and rounded to an integer
//! (`Shell::update_output_image_copy_cursor_position`, and the same conversion again when
//! a session is first created). So the conversion back to global logical pixels is
//! `logical_origin + buffer / scale`, which is what [`OutputInfo::buffer_to_global`]
//! does.
//!
//! Two consequences worth knowing. The value is rounded to whole physical pixels, so on a
//! scaled output it quantises to less than one logical pixel. And the scale recovered from
//! `zxdg_output_v1` is itself the ratio of a rounded logical size to the mode size, so it
//! differs from the compositor's true fractional scale by up to ~0.03%; at the far edge of
//! HDMI-A-1 that is under half a logical pixel.
//!
//! [`OutputInfo::buffer_to_global`]: crate::OutputInfo::buffer_to_global

use std::io::ErrorKind;
use std::time::{Duration, Instant};

use gaze_core::GlobalPx;
use rustix::event::{PollFd, PollFlags, poll};
use rustix::time::Timespec;
use wayland_client::backend::WaylandError;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::wl_pointer::WlPointer;
use wayland_client::protocol::wl_registry::{Event as RegistryEvent, WlRegistry};
use wayland_client::protocol::wl_seat::{Capability, Event as SeatEvent, WlSeat};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop,
};
use wayland_protocols::ext::image_capture_source::v1::client::ext_image_capture_source_v1::ExtImageCaptureSourceV1;
use wayland_protocols::ext::image_capture_source::v1::client::ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_cursor_session_v1::{
    Event as CursorEvent, ExtImageCopyCaptureCursorSessionV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_session_v1::{
    Event as SessionEvent, ExtImageCopyCaptureSessionV1,
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

/// `wl_seat` version to bind. Version 5 is enough for `capabilities`, which is all this
/// module reads the seat for.
const WL_SEAT_VERSION: u32 = 5;

/// One pointer position report, with the compositor's raw numbers kept alongside the
/// converted result so callers can sanity-check the unit convention themselves.
#[derive(Clone, Debug, PartialEq)]
pub struct CursorReport {
    /// Connector the pointer was on when the position was reported.
    pub output   : String,
    /// Position exactly as `ext_image_copy_capture_cursor_session_v1.position` sent it,
    /// in the capture source's buffer pixels, which are physical and not logical.
    pub buffer_x : i32,
    pub buffer_y : i32,
    /// `buffer_x`/`buffer_y` scaled down to logical pixels and offset by the output's
    /// logical origin.
    pub global   : GlobalPx,
    /// The cursor image's hotspot, in the image's own pixels, from the session's last
    /// `hotspot` event. `None` until one has arrived for this output.
    pub hotspot  : Option<(i32, i32)>,
    /// The cursor image's size, from the derived capture session's `buffer_size`. `None`
    /// until it has arrived. With `hotspot` this is the cursor's *shape* without copying
    /// a pixel: an arrow's hotspot sits in a corner, an I-beam's in the centre, a hand's
    /// at the top edge, and the compositor re-sends both whenever the client under the
    /// pointer changes the cursor.
    pub image_px : Option<(u32, u32)>,
}

/// Tracks the pointer's position in global logical pixels.
///
/// Not `Send`, for the same reason `Capture` is not: the event queue and its proxies
/// belong to the thread that built them.
pub struct CursorTracker {
    /// Kept alive because every proxy borrows the connection's backend, and because
    /// `wait_position` polls its file descriptor.
    conn  : Connection,
    queue : EventQueue<CursorState>,
    qh    : QueueHandle<CursorState>,
    state : CursorState,
}

// --- CursorTracker ---

impl CursorTracker {
    /// Connects to the compositor and opens a cursor session on every current output.
    ///
    /// Fails when the compositor is missing one of the capture globals, the seat, or a
    /// pointer capability on that seat.
    pub fn connect() -> Result<Self, CaptureError> {
        let conn = Connection::connect_to_env()
            .map_err(|e| CaptureError::Connect { detail: e.to_string() })?;

        let (globals, mut queue) = registry_queue_init::<CursorState>(&conn)
            .map_err(|e| CaptureError::Connect { detail: e.to_string() })?;

        let qh = queue.handle();

        let source_mgr: ExtOutputImageCaptureSourceManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .map_err(|_| CaptureError::MissingGlobal {
                interface: "ext_output_image_capture_source_manager_v1",
            })?;

        let copy_mgr: ExtImageCopyCaptureManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .map_err(|_| CaptureError::MissingGlobal {
                interface: "ext_image_copy_capture_manager_v1",
            })?;

        let xdg_mgr: ZxdgOutputManagerV1 = globals
            .bind(&qh, 1..=XDG_OUTPUT_VERSION, ())
            .map_err(|_| CaptureError::MissingGlobal { interface: "zxdg_output_manager_v1" })?;

        let seat: WlSeat = globals
            .bind(&qh, 1..=WL_SEAT_VERSION, ())
            .map_err(|_| CaptureError::MissingGlobal { interface: "wl_seat" })?;

        let mut state = CursorState {
            source_mgr : source_mgr,
            copy_mgr   : copy_mgr,
            xdg_mgr    : xdg_mgr,
            seat       : seat,
            pointer    : None,
            outputs    : Vec::new(),
            sessions   : Vec::new(),
            inside     : None,
            last       : None,
            report     : None,
            updates    : 0,
            events     : Vec::new(),
        };

        let registry = globals.registry().clone();

        for global in globals.contents().clone_list() {
            if is_output(&global.interface) {
                let entry = bind_output(&qh, &registry, &state.xdg_mgr, global.name, global.version);
                state.outputs.push(entry);
            }
        }

        // First round trip brings the seat capabilities and the output properties; the
        // pointer cannot be requested before the capability is known.
        queue
            .roundtrip(&mut state)
            .map_err(|e| CaptureError::Protocol { detail: e.to_string() })?;
        queue
            .roundtrip(&mut state)
            .map_err(|e| CaptureError::Protocol { detail: e.to_string() })?;

        if state.pointer.is_none() {
            return Err(CaptureError::MissingGlobal { interface: "wl_seat pointer capability" });
        }

        let mut tracker = Self {
            conn  : conn,
            queue : queue,
            qh    : qh,
            state : state,
        };

        tracker.state.sync_sessions(&tracker.qh);

        // Let the compositor answer the fresh sessions, so a caller that polls
        // immediately after `connect` already has whatever it was going to send.
        tracker
            .queue
            .roundtrip(&mut tracker.state)
            .map_err(|e| CaptureError::Protocol { detail: e.to_string() })?;

        Ok(tracker)
    }

    /// Last known pointer position in global logical pixels, without blocking.
    ///
    /// Drains whatever the compositor has already sent and returns immediately. `None`
    /// means the pointer has not been reported on any output yet, or has left every output
    /// this tracker watches.
    pub fn position(&mut self) -> Result<Option<GlobalPx>, CaptureError> {
        self.pump()?;

        Ok(self.state.last)
    }

    /// Blocks until the compositor reports a new pointer position, or `timeout` elapses.
    ///
    /// Returns the new position on success. Returns `Ok(None)` both on timeout and when
    /// the new event was a `leave`, because in either case there is no current position to
    /// act on; use [`position`](Self::position) afterwards if the distinction matters.
    pub fn wait_position(&mut self, timeout: Duration)
        -> Result<Option<GlobalPx>, CaptureError>
    {
        let deadline = Instant::now() + timeout;
        let start    = self.state.updates;

        loop {
            self.pump()?;

            if self.state.updates != start {
                return Ok(self.state.last);
            }

            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }

            self.poll_socket(deadline - now)?;
        }
    }

    /// Details of the most recent position report, including the compositor's raw numbers.
    ///
    /// Exposed so the CLI can show what units cosmic-comp actually sends rather than
    /// asking a caller to trust the conversion.
    pub fn last_report(&self) -> Option<CursorReport> {
        self.state.report.clone()
    }

    /// Names of the outputs this tracker currently holds a cursor session on.
    pub fn tracked_outputs(&self) -> Vec<String> {
        self.state
            .sessions
            .iter()
            .filter_map(|s| {
                self.state
                    .outputs
                    .iter()
                    .find(|o| o.global_name == s.global_name)
                    .and_then(|o| o.name.clone())
            })
            .collect()
    }

    /// Human-readable log of the cursor-session events seen so far, oldest first.
    ///
    /// Kept because a compositor that implements only part of this protocol is best
    /// diagnosed by seeing which events do arrive.
    pub fn event_log(&self) -> &[String] {
        &self.state.events
    }
}

impl CursorTracker {
    /// Reads and dispatches everything the socket has without blocking.
    ///
    /// One socket read per call is enough: a read drains the whole kernel buffer, and the
    /// queue is dispatched both before and after it so events that were already parsed are
    /// not left sitting until the next call.
    fn pump(&mut self) -> Result<(), CaptureError> {
        self.queue.flush().map_err(wayland_err)?;
        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(|e| CaptureError::Protocol { detail: e.to_string() })?;

        // `prepare_read` returns None when events are already queued, in which case the
        // dispatch above and below is all that is needed.
        if let Some(guard) = self.queue.prepare_read() {
            match guard.read() {
                Ok(_) => {}

                // Nothing to read is the normal case for a still pointer.
                Err(WaylandError::Io(e)) if e.kind() == ErrorKind::WouldBlock => {}

                Err(e) => {
                    return Err(wayland_err(e));
                }
            }
        }

        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(|e| CaptureError::Protocol { detail: e.to_string() })?;

        // Outputs that appeared or vanished in this batch get their sessions fixed up.
        self.state.sync_sessions(&self.qh);
        self.queue.flush().map_err(wayland_err)?;

        Ok(())
    }

    /// Waits up to `timeout` for the wayland socket to become readable.
    fn poll_socket(&self, timeout: Duration) -> Result<(), CaptureError> {
        let mut fds = [PollFd::new(&self.conn, PollFlags::IN)];
        let spec    = Timespec {
            tv_sec  : timeout.as_secs() as i64,
            tv_nsec : timeout.subsec_nanos() as _,
        };

        match poll(&mut fds, Some(&spec)) {
            Ok(_) => Ok(()),

            // A signal during the wait is not an error; the caller's deadline loop retries.
            Err(e) if e == rustix::io::Errno::INTR => Ok(()),

            Err(e) => Err(CaptureError::Protocol { detail: format!("poll: {e}") }),
        }
    }
}

// --- CursorState ---

/// Everything the cursor event handlers read and write.
struct CursorState {
    source_mgr : ExtOutputImageCaptureSourceManagerV1,
    copy_mgr   : ExtImageCopyCaptureManagerV1,
    xdg_mgr    : ZxdgOutputManagerV1,
    seat       : WlSeat,
    /// `None` until the seat announces a pointer capability.
    pointer    : Option<WlPointer>,
    outputs    : Vec<OutputEntry>,
    /// One cursor session per output that has usable geometry.
    sessions   : Vec<CursorSession>,
    /// Registry name of the output the pointer was last seen entering.
    inside     : Option<u32>,
    /// Last converted position, cleared when the pointer leaves the output it was on.
    last       : Option<GlobalPx>,
    /// Raw and converted form of the last position event.
    report     : Option<CursorReport>,
    /// Bumped on every enter, leave and position event, so `wait_position` can tell a new
    /// report from a repeat of the old one.
    updates    : u64,
    /// Bounded log of cursor-session events, for diagnosing partial implementations.
    events     : Vec<String>,
}

/// The per-output objects that make up one cursor session.
struct CursorSession {
    /// Registry name of the output this session follows.
    global_name : u32,
    source      : ExtImageCaptureSourceV1,
    session     : ExtImageCopyCaptureCursorSessionV1,
    /// The frame session derived from the cursor session. Never used to capture, but some
    /// compositors only start tracking the pointer once it exists, and its `buffer_size`
    /// is the cursor image's size.
    capture     : ExtImageCopyCaptureSessionV1,
    /// Last `hotspot` event on this output.
    hotspot     : Option<(i32, i32)>,
    /// Last `buffer_size` event on the derived capture session.
    image_px    : Option<(u32, u32)>,
}

impl CursorState {
    /// Largest number of cursor-session events kept in `events`.
    const EVENT_LOG_CAP: usize = 64;

    /// Opens sessions for outputs that gained geometry and closes those that went away.
    fn sync_sessions(&mut self, qh: &QueueHandle<Self>) {
        let Some(pointer) = self.pointer.clone() else {
            return;
        };

        // Drop sessions whose output is gone. Outputs are removed from `outputs` by the
        // registry handler, which cannot reach the session list without this pass.
        let live: Vec<u32> = self.outputs.iter().map(|o| o.global_name).collect();
        let mut kept = Vec::with_capacity(self.sessions.len());

        for s in self.sessions.drain(..) {
            if live.contains(&s.global_name) {
                kept.push(s);
                continue;
            }

            s.capture.destroy();
            s.session.destroy();
            s.source.destroy();
        }

        self.sessions = kept;

        // Open a session for every usable output that does not have one yet.
        for entry in &self.outputs {
            if entry.info().is_none() {
                continue;
            }

            if self.sessions.iter().any(|s| s.global_name == entry.global_name) {
                continue;
            }

            let source  = self.source_mgr.create_source(&entry.wl_output, qh, ());
            let session = self.copy_mgr.create_pointer_cursor_session(
                &source,
                &pointer,
                qh,
                entry.global_name,
            );
            let capture = session.get_capture_session(qh, entry.global_name);

            self.sessions.push(CursorSession {
                global_name : entry.global_name,
                source      : source,
                session     : session,
                capture     : capture,
                hotspot     : None,
                image_px    : None,
            });
        }
    }

    /// The session following one output.
    fn session(&self, global_name: u32) -> Option<&CursorSession> {
        self.sessions.iter().find(|s| s.global_name == global_name)
    }

    /// Copies one output's cursor image details into the current report, when the
    /// pointer is on that output. A shape change without motion arrives as `hotspot` and
    /// `buffer_size` alone, with no `position` to rebuild the report from.
    fn refresh_cursor_image(&mut self, global_name: u32) {
        if self.inside != Some(global_name) {
            return;
        }

        let (hotspot, image_px) = self.session(global_name)
            .map_or((None, None), |s| (s.hotspot, s.image_px));

        if let Some(report) = self.report.as_mut() {
            report.hotspot  = hotspot;
            report.image_px = image_px;
        }
    }

    /// Appends to the bounded diagnostic log.
    fn log_event(&mut self, global_name: u32, text: String) {
        let name = self
            .outputs
            .iter()
            .find(|o| o.global_name == global_name)
            .and_then(|o| o.name.clone())
            .unwrap_or_else(|| format!("global {global_name}"));

        if self.events.len() == Self::EVENT_LOG_CAP {
            self.events.remove(0);
        }

        self.events.push(format!("{name}: {text}"));
    }

    /// Drops an output and any session following it.
    fn remove_output(&mut self, global_name: u32) {
        let Some(pos) = self.outputs.iter().position(|o| o.global_name == global_name) else {
            return;
        };

        release_output(self.outputs.remove(pos));

        // Forget a position that referred to the output that just vanished.
        if self.inside == Some(global_name) {
            self.inside = None;
            self.last   = None;
            self.updates += 1;
        }
    }
}

// --- Dispatch ---

impl Dispatch<WlRegistry, GlobalListContents> for CursorState {
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

impl Dispatch<WlSeat, ()> for CursorState {
    /// Requests a `wl_pointer` as soon as the seat says it has one.
    fn event(
        state    : &mut Self,
        _proxy   : &WlSeat,
        event    : SeatEvent,
        _data    : &(),
        _conn    : &Connection,
        qh       : &QueueHandle<Self>,
    ) {
        let SeatEvent::Capabilities { capabilities: WEnum::Value(caps) } = event else {
            return;
        };

        if caps.contains(Capability::Pointer) && state.pointer.is_none() {
            state.pointer = Some(state.seat.get_pointer(qh, ()));
        }
    }
}

impl Dispatch<WlOutput, u32> for CursorState {
    /// Records the connector name and the current mode's physical size.
    fn event(
        state    : &mut Self,
        _proxy   : &WlOutput,
        event    : <WlOutput as Proxy>::Event,
        data     : &u32,
        _conn    : &Connection,
        _qh      : &QueueHandle<Self>,
    ) {
        let Some(entry) = state.outputs.iter_mut().find(|o| o.global_name == *data) else {
            return;
        };

        apply_wl_output(entry, event);
    }
}

impl Dispatch<ZxdgOutputV1, u32> for CursorState {
    /// Records the logical rectangle, and the name when `wl_output` did not supply one.
    fn event(
        state    : &mut Self,
        _proxy   : &ZxdgOutputV1,
        event    : XdgOutputEvent,
        data     : &u32,
        _conn    : &Connection,
        _qh      : &QueueHandle<Self>,
    ) {
        let Some(entry) = state.outputs.iter_mut().find(|o| o.global_name == *data) else {
            return;
        };

        apply_xdg_output(entry, event);
    }
}

impl Dispatch<ExtImageCopyCaptureCursorSessionV1, u32> for CursorState {
    /// Converts pointer reports for one output into global logical pixels.
    fn event(
        state    : &mut Self,
        _proxy   : &ExtImageCopyCaptureCursorSessionV1,
        event    : CursorEvent,
        data     : &u32,
        _conn    : &Connection,
        _qh      : &QueueHandle<Self>,
    ) {
        let global_name = *data;

        match event {
            CursorEvent::Enter => {
                state.inside  = Some(global_name);
                state.updates += 1;
                state.log_event(global_name, "enter".to_string());
            }

            CursorEvent::Leave => {
                // Only clear when the pointer left the output it was actually on: reports
                // for other outputs can interleave.
                if state.inside == Some(global_name) {
                    state.inside = None;
                    state.last   = None;
                    state.report = None;
                }

                state.updates += 1;
                state.log_event(global_name, "leave".to_string());
            }

            CursorEvent::Position { x, y } => {
                let converted = state
                    .outputs
                    .iter()
                    .find(|o| o.global_name == global_name)
                    .and_then(|o| {
                        o.buffer_to_global(f64::from(x), f64::from(y))
                            .zip(o.name.clone())
                    });

                if let Some((global, name)) = converted {
                    let (hotspot, image_px) = state.session(global_name)
                        .map_or((None, None), |s| (s.hotspot, s.image_px));

                    state.inside = Some(global_name);
                    state.last   = Some(global);
                    state.report = Some(CursorReport {
                        output   : name,
                        buffer_x : x,
                        buffer_y : y,
                        global   : global,
                        hotspot  : hotspot,
                        image_px : image_px,
                    });
                }

                state.updates += 1;
                state.log_event(global_name, format!("position {x},{y}"));
            }

            // The hotspot is the cursor image's own offset, not part of the pointer
            // position: cosmic-comp sends the pointer location itself in `position`.
            // It is kept as half of the cursor's shape (see `CursorReport::image_px`)
            // and logged so a caller debugging an offset can see it.
            CursorEvent::Hotspot { x, y } => {
                if let Some(s) = state.sessions.iter_mut().find(|s| s.global_name == global_name) {
                    s.hotspot = Some((x, y));
                }

                state.refresh_cursor_image(global_name);
                state.log_event(global_name, format!("hotspot {x},{y}"));
            }

            _ => {}
        }
    }
}

delegate_noop!(CursorState: ignore WlPointer);
delegate_noop!(CursorState: ignore ExtImageCaptureSourceV1);
delegate_noop!(CursorState: ignore ExtOutputImageCaptureSourceManagerV1);
delegate_noop!(CursorState: ignore ExtImageCopyCaptureManagerV1);
impl Dispatch<ExtImageCopyCaptureSessionV1, u32> for CursorState {
    /// Keeps the cursor image's size; the session is never asked for a frame.
    fn event(
        state    : &mut Self,
        _proxy   : &ExtImageCopyCaptureSessionV1,
        event    : SessionEvent,
        data     : &u32,
        _conn    : &Connection,
        _qh      : &QueueHandle<Self>,
    ) {
        let global_name = *data;

        if let SessionEvent::BufferSize { width, height } = event {
            if let Some(s) = state.sessions.iter_mut().find(|s| s.global_name == global_name) {
                s.image_px = Some((width, height));
            }

            state.refresh_cursor_image(global_name);
            state.log_event(global_name, format!("buffer_size {width}x{height}"));
        }
    }
}
delegate_noop!(CursorState: ignore ZxdgOutputManagerV1);

// --- Helpers ---

/// Wraps a backend error as a protocol error, which is the only shape callers care about.
fn wayland_err(e: WaylandError) -> CaptureError {
    CaptureError::Protocol { detail: e.to_string() }
}
