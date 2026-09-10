//! Per-output screen capture over `ext_image_copy_capture_v1`.
//!
//! ## Why a fresh session per capture
//!
//! `ext_image_copy_capture_frame_v1.capture` is documented to block indefinitely on every
//! frame after the first one in a session, waiting for the source content to change. A
//! persistent per-output session would therefore hang on a static desktop, which is
//! exactly the state this crate is asked to screenshot. A session created immediately
//! before each capture always yields its first frame, so every call returns promptly and
//! bounded by the `set_timeout` deadline. The cost is one round trip for the buffer
//! constraints and one shm allocation per call, both small next to the memcpy of a
//! 3840x1600 frame.

use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_output::{Transform, WlOutput};
use wayland_client::protocol::wl_registry::{Event as RegistryEvent, WlRegistry};
use wayland_client::protocol::wl_shm::{Format, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop,
};
use wayland_protocols::ext::image_capture_source::v1::client::ext_image_capture_source_v1::ExtImageCaptureSourceV1;
use wayland_protocols::ext::image_capture_source::v1::client::ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_frame_v1::{
    Event as FrameEvent, ExtImageCopyCaptureFrameV1, FailureReason,
};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::{
    ExtImageCopyCaptureManagerV1, Options,
};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_session_v1::{
    Event as SessionEvent, ExtImageCopyCaptureSessionV1,
};
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_manager_v1::ZxdgOutputManagerV1;
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_v1::{
    Event as XdgOutputEvent, ZxdgOutputV1,
};

use crate::frame::{Frame, OutputInfo};
use crate::outputs::{
    OutputEntry, XDG_OUTPUT_VERSION, apply_wl_output, apply_xdg_output, bind_output, is_output,
    release_output,
};
use crate::shm::ShmBuffer;

/// How long to wait for the compositor to answer a session or frame request before giving
/// up. Generous: a busy cosmic-comp answers in single-digit milliseconds, but an output in
/// the middle of a mode set can stall, and hanging forever is worse than an `Err`.
const DEFAULT_TIMEOUT: Duration = Duration::from_millis(2000);

/// A connection to the compositor with the capture globals bound.
///
/// Not `Send`: the wayland event queue and every proxy in it belong to the thread that
/// built them. Capture on a background thread means constructing a `Capture` there.
pub struct Capture {
    /// Kept alive because every proxy in the queue borrows the connection's backend.
    _conn   : Connection,
    queue   : EventQueue<State>,
    qh      : QueueHandle<State>,
    state   : State,
    timeout : Duration,
}

// --- Capture ---

impl Capture {
    /// Connects to the compositor named by `WAYLAND_DISPLAY` and binds the capture globals.
    ///
    /// Fails when the socket is missing or when the compositor does not implement one of
    /// `ext_image_copy_capture_manager_v1`, `ext_output_image_capture_source_manager_v1`,
    /// `zxdg_output_manager_v1` or `wl_shm`.
    pub fn connect() -> Result<Self, CaptureError> {
        let conn = Connection::connect_to_env()
            .map_err(|e| CaptureError::Connect { detail: e.to_string() })?;

        let (globals, mut queue) = registry_queue_init::<State>(&conn)
            .map_err(|e| CaptureError::Connect { detail: e.to_string() })?;

        let qh = queue.handle();

        // Bind the four globals we cannot work without, naming each one in the error so a
        // missing protocol is obvious from the message alone.
        let shm: WlShm = globals
            .bind(&qh, 1..=2, ())
            .map_err(|_| CaptureError::MissingGlobal { interface: "wl_shm" })?;

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

        let mut state = State {
            shm         : shm,
            source_mgr  : source_mgr,
            copy_mgr    : copy_mgr,
            xdg_mgr     : xdg_mgr,
            outputs     : Vec::new(),
            session     : SessionState::default(),
            frame       : FrameState::default(),
        };

        // The registry callback in `registry_queue_init` already saw every global, but it
        // had no state to record them into, so walk the cached list once here.
        let registry = globals.registry().clone();

        for global in globals.contents().clone_list() {
            if global.interface == WlOutput::interface().name {
                state.add_output(&qh, &registry, global.name, global.version);
            }
        }

        // Two round trips: the first delivers wl_output and xdg_output properties, the
        // second catches anything the compositor queued in response.
        queue
            .roundtrip(&mut state)
            .map_err(|e| CaptureError::Protocol { detail: e.to_string() })?;
        queue
            .roundtrip(&mut state)
            .map_err(|e| CaptureError::Protocol { detail: e.to_string() })?;

        Ok(Self {
            _conn   : conn,
            queue   : queue,
            qh      : qh,
            state   : state,
            timeout : DEFAULT_TIMEOUT,
        })
    }

    /// Overrides the per-request deadline used when waiting on the compositor.
    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    /// Current outputs, re-queried from the compositor on every call.
    ///
    /// Outputs on this desk come and go (HDMI-A-1 drops off the list and returns), so the
    /// result of a previous call must never be cached across a capture. Outputs whose
    /// name or logical geometry has not arrived yet are skipped rather than guessed at.
    pub fn outputs(&mut self) -> Vec<OutputInfo> {
        if let Err(e) = self.refresh() {
            tracing::warn!("output refresh failed: {e}");
        }

        self.state.outputs.iter().filter_map(OutputEntry::info).collect()
    }

    /// Captures one output by connector name, for example `"DP-1"`.
    ///
    /// Returns `Err(CaptureError::NoSuchOutput)` when the output is not currently in the
    /// compositor's list, which is the normal outcome for a monitor that has just gone
    /// away rather than a bug.
    pub fn capture_output(&mut self, name: &str) -> Result<Frame, CaptureError> {
        self.refresh()?;

        let Some(index) = self.state.outputs.iter().position(|o| o.name.as_deref() == Some(name))
        else {
            return Err(CaptureError::NoSuchOutput { name: name.to_string() });
        };

        self.capture_index(index)
    }

    /// Captures every current output in list order, one result per output.
    ///
    /// A failure on one output does not abort the others: a monitor disappearing mid-sweep
    /// yields an `Err` in its slot and leaves the rest intact.
    pub fn capture_all(&mut self) -> Vec<Result<Frame, CaptureError>> {
        if let Err(e) = self.refresh() {
            return vec![Err(e)];
        }

        // Snapshot the names first: capturing re-enters `refresh` and may reorder or
        // shorten `state.outputs` underneath us.
        let names: Vec<String> = self
            .state
            .outputs
            .iter()
            .filter(|o| o.info().is_some())
            .filter_map(|o| o.name.clone())
            .collect();

        names.into_iter().map(|n| self.capture_output(&n)).collect()
    }
}

impl Capture {
    /// Pulls pending registry and output events so the output list matches the compositor.
    ///
    /// One round trip is enough: globals added or removed since the last call are
    /// delivered by it, and any `wl_output`/`zxdg_output_v1` properties for a freshly
    /// bound output arrive in the same batch because the binds happen inside the registry
    /// callback that the round trip drains.
    fn refresh(&mut self) -> Result<(), CaptureError> {
        self.queue
            .roundtrip(&mut self.state)
            .map_err(|e| CaptureError::Protocol { detail: e.to_string() })?;

        // A newly appeared output has its xdg_output request sent but not yet answered, so
        // spend a second round trip only when something is still incomplete.
        let incomplete = self.state.outputs.iter().any(|o| o.info().is_none());
        if incomplete {
            self.queue
                .roundtrip(&mut self.state)
                .map_err(|e| CaptureError::Protocol { detail: e.to_string() })?;
        }

        Ok(())
    }

    /// Runs one full capture against `state.outputs[index]`.
    ///
    /// The caller must have refreshed the output list immediately before, because the
    /// index is only meaningful against the current list.
    fn capture_index(&mut self, index: usize) -> Result<Frame, CaptureError> {
        let entry = &self.state.outputs[index];
        let Some(info) = entry.info() else {
            return Err(CaptureError::NoSuchOutput {
                name: entry.name.clone().unwrap_or_default(),
            });
        };

        let wl_output = entry.wl_output.clone();

        // 1. Open a source and a session, then wait for the buffer constraints. The
        //    options bitfield is empty, which is what keeps the cursor out of the frame.
        self.state.session = SessionState::default();

        let source  = self.state.source_mgr.create_source(&wl_output, &self.qh, ());
        let session = self.state.copy_mgr.create_session(&source, Options::empty(), &self.qh, ());

        let constraints = self.wait_for(|s| s.session.done || s.session.stopped);
        let result = constraints.and_then(|()| self.run_capture(&session, &info));

        // The session and source are per-call, so tear them down whatever happened.
        session.destroy();
        source.destroy();

        result
    }

    /// Allocates a buffer matching the session constraints, captures into it and converts.
    ///
    /// Split out of `capture_index` so the session teardown above runs on every path.
    fn run_capture(
        &mut self,
        session : &ExtImageCopyCaptureSessionV1,
        info    : &OutputInfo,
    )
        -> Result<Frame, CaptureError>
    {
        if self.state.session.stopped {
            return Err(CaptureError::SessionStopped { output: info.name.clone() });
        }

        let width  = self.state.session.width;
        let height = self.state.session.height;

        if width == 0 || height == 0 {
            return Err(CaptureError::BufferSize { len: 0 });
        }

        // 2. Pick a format we can convert from. Order of preference is opaque before
        //    alpha, because an opaque source spares us a per-pixel alpha overwrite.
        let format = pick_format(&self.state.session.formats).ok_or_else(|| {
            CaptureError::NoUsableFormat { offered: self.state.session.formats.clone() }
        })?;

        tracing::debug!(
            "{}: {}x{} shm format {:?} chosen from {:?}",
            info.name, width, height, format, self.state.session.formats,
        );

        // 3. Back the buffer with a memfd and wrap it in a single-buffer pool.
        let stride = width as usize * 4;
        let len    = stride * height as usize;
        let shm    = ShmBuffer::new(len)?;

        let pool = self.state.shm.create_pool(
            shm.fd().as_fd(),
            shm.len() as i32,
            &self.qh,
            (),
        );
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            format,
            &self.qh,
            (),
        );

        // 4. Ask for the frame. Damage is the whole buffer: it has never been captured
        //    into, so the compositor must fill all of it.
        self.state.frame = FrameState::default();

        let frame = session.create_frame(&self.qh, ());
        frame.attach_buffer(&buffer);
        frame.damage_buffer(0, 0, width as i32, height as i32);
        frame.capture();

        let waited = self.wait_for(|s| s.frame.done);

        let outcome = waited.and_then(|()| {
            if let Some(reason) = self.state.frame.failure {
                return Err(CaptureError::FrameFailed {
                    output : info.name.clone(),
                    reason : reason,
                });
            }

            // Every output on this desk is untransformed, and rotating the buffer here
            // would silently disagree with the logical rectangle the boxes are expressed
            // in, so refuse instead of producing a mismatched frame.
            match self.state.frame.transform {
                Transform::Normal => Ok(()),
                other             => Err(CaptureError::UnsupportedTransform {
                    output    : info.name.clone(),
                    transform : format!("{other:?}"),
                }),
            }
        });

        let rgba = outcome.map(|()| to_rgba(shm.bytes(), width, height, format));

        // The compositor is done with the buffer once `ready` (or `failed`) arrived, so
        // releasing the pool here is safe and keeps the memfd from outliving the call.
        frame.destroy();
        buffer.destroy();
        pool.destroy();

        let t_s = self.state.frame.t_s.unwrap_or_else(monotonic_now_s);

        Ok(Frame {
            output  : info.name.clone(),
            logical : info.logical,
            width   : width,
            height  : height,
            rgba    : rgba?,
            t_s     : t_s,
        })
    }

    /// Dispatches events until `ready` holds or the deadline expires.
    ///
    /// The wait is on the connection's socket with `poll`, so a compositor that goes
    /// completely silent (an output that has gone to sleep never delivers its frame)
    /// ends in `Timeout` rather than blocking the thread for as long as the output is
    /// off. Everything already read is dispatched before waiting, and anything that
    /// arrives is read and dispatched in one step.
    fn wait_for(&mut self, ready: impl Fn(&State) -> bool) -> Result<(), CaptureError> {
        let deadline = Instant::now() + self.timeout;
        let protocol = |e: &dyn std::fmt::Display| CaptureError::Protocol { detail: e.to_string() };

        while !ready(&self.state) {
            self.queue.dispatch_pending(&mut self.state).map_err(|e| protocol(&e))?;

            if ready(&self.state) {
                break;
            }

            let remaining = deadline.saturating_duration_since(Instant::now());

            if remaining.is_zero() {
                return Err(CaptureError::Timeout { ms: self.timeout.as_millis() as u64 });
            }

            self.queue.flush().map_err(|e| protocol(&e))?;

            // `None` means events landed in the buffer between the dispatch above and
            // now; go round and dispatch them.
            let Some(guard) = self.queue.prepare_read() else {
                continue;
            };

            let fd    = guard.connection_fd();
            let flags = rustix::event::PollFlags::IN;
            let mut fds = [rustix::event::PollFd::new(&fd, flags)];

            let woke = rustix::event::poll(&mut fds, Some(&rustix::time::Timespec::try_from(remaining).map_err(|e| protocol(&e))?))
                .map_err(|e| protocol(&e))?;

            if woke == 0 {
                // Nothing arrived in time; the guard is dropped without reading.
                return Err(CaptureError::Timeout { ms: self.timeout.as_millis() as u64 });
            }

            guard.read().map_err(|e| protocol(&e))?;
        }

        Ok(())
    }
}

// --- State ---

/// Everything the event handlers read and write. One per `Capture`.
struct State {
    shm        : WlShm,
    source_mgr : ExtOutputImageCaptureSourceManagerV1,
    copy_mgr   : ExtImageCopyCaptureManagerV1,
    xdg_mgr    : ZxdgOutputManagerV1,
    /// Live outputs in registry order. Entries are removed on `global_remove`.
    outputs    : Vec<OutputEntry>,
    /// Constraints for the session currently being set up. Reset before each capture.
    session    : SessionState,
    /// Result of the frame currently in flight. Reset before each capture.
    frame      : FrameState,
}

impl State {
    /// Binds a newly advertised `wl_output` and asks for its xdg counterpart.
    ///
    /// Called both from the initial global walk and from the registry `global` event, so
    /// an output that appears while the process runs is picked up without a reconnect.
    fn add_output(
        &mut self,
        qh          : &QueueHandle<Self>,
        registry    : &WlRegistry,
        global_name : u32,
        version     : u32,
    ) {
        if self.outputs.iter().any(|o| o.global_name == global_name) {
            return;
        }

        let entry = bind_output(qh, registry, &self.xdg_mgr, global_name, version);
        self.outputs.push(entry);
    }

    /// Drops an output that the compositor has taken away.
    fn remove_output(&mut self, global_name: u32) {
        let Some(pos) = self.outputs.iter().position(|o| o.global_name == global_name) else {
            return;
        };

        release_output(self.outputs.remove(pos));
    }
}

// --- Session and frame state ---

/// Buffer constraints advertised by a capture session, accumulated until `done`.
#[derive(Default)]
struct SessionState {
    width   : u32,
    height  : u32,
    /// shm formats the compositor will accept, in the order it offered them.
    formats : Vec<Format>,
    done    : bool,
    /// Set by `stopped`, which also ends any wait: the session will never send `done`.
    stopped : bool,
}

/// Outcome of the frame currently in flight.
struct FrameState {
    /// Set by both `ready` and `failed`; it is what the capture wait blocks on.
    done      : bool,
    /// `Some` only when the compositor sent `failed`.
    failure   : Option<FailureReason>,
    /// `presentation_time` converted to `CLOCK_MONOTONIC` seconds, when it was sent.
    t_s       : Option<f64>,
    /// Transform the compositor applied to the buffer. `Normal` until told otherwise,
    /// which is also the correct assumption when the event is not sent at all.
    transform : Transform,
}

impl Default for FrameState {
    fn default() -> Self {
        Self {
            done      : false,
            failure   : None,
            t_s       : None,
            transform : Transform::Normal,
        }
    }
}

// --- Pixel conversion ---

/// Chooses the shm format to request from the offered set.
///
/// Opaque formats come first because they need no alpha fix-up, and the packed 32-bit
/// layouts are the only ones this crate knows how to unpack.
fn pick_format(offered: &[Format]) -> Option<Format> {
    const PREFERRED: [Format; 4] = [
        Format::Xrgb8888,
        Format::Xbgr8888,
        Format::Argb8888,
        Format::Abgr8888,
    ];

    PREFERRED.into_iter().find(|f| offered.contains(f))
}

/// Converts a packed 32-bit shm buffer to tightly packed RGBA8.
///
/// wl_shm names its formats in little-endian word order, so `xrgb8888` is B, G, R, X in
/// memory. Alpha is forced opaque for the `x` variants because their fourth byte is
/// undefined and a detector fed a zero alpha would see a fully transparent screen.
///
/// Panics if `src` is shorter than `width * height * 4`.
fn to_rgba(src: &[u8], width: u32, height: u32, format: Format) -> Vec<u8> {
    let pixels = width as usize * height as usize;
    let mut out = vec![0u8; pixels * 4];

    // Byte offsets of red and blue within each source pixel, and whether alpha is real.
    let (r_off, b_off, opaque) = match format {
        Format::Xrgb8888 => (2, 0, true),
        Format::Argb8888 => (2, 0, false),
        Format::Xbgr8888 => (0, 2, true),
        Format::Abgr8888 => (0, 2, false),
        // `pick_format` never returns anything else.
        _                => unreachable!("unsupported shm format {format:?}"),
    };

    for i in 0..pixels {
        let s = i * 4;
        let d = i * 4;

        out[d    ] = src[s + r_off];
        out[d + 1] = src[s + 1];
        out[d + 2] = src[s + b_off];
        out[d + 3] = if opaque { 255 } else { src[s + 3] };
    }

    out
}

/// Reads `CLOCK_MONOTONIC` as seconds, the same clock the compositor timestamps frames in.
fn monotonic_now_s() -> f64 {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);

    t.tv_sec as f64 + t.tv_nsec as f64 * 1e-9
}

// --- Dispatch ---

impl Dispatch<WlRegistry, GlobalListContents> for State {
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
            RegistryEvent::Global { name, interface, version }
                if is_output(&interface) =>
            {
                state.add_output(qh, registry, name, version);
            }

            RegistryEvent::GlobalRemove { name } => {
                state.remove_output(name);
            }

            _ => {}
        }
    }
}

impl Dispatch<WlOutput, u32> for State {
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

impl Dispatch<ZxdgOutputV1, u32> for State {
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

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for State {
    /// Accumulates buffer constraints until `done`, or gives up on `stopped`.
    fn event(
        state    : &mut Self,
        _proxy   : &ExtImageCopyCaptureSessionV1,
        event    : SessionEvent,
        _data    : &(),
        _conn    : &Connection,
        _qh      : &QueueHandle<Self>,
    ) {
        match event {
            SessionEvent::BufferSize { width, height } => {
                state.session.width  = width;
                state.session.height = height;
            }

            // An unknown format code is one we could not convert anyway, so skip it.
            SessionEvent::ShmFormat { format: WEnum::Value(f) } => {
                state.session.formats.push(f);
            }

            SessionEvent::Done => {
                state.session.done = true;
            }

            SessionEvent::Stopped => {
                state.session.stopped = true;
            }

            // dmabuf_device and dmabuf_format are irrelevant: this crate is shm only.
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for State {
    /// Latches the frame outcome, its transform and its presentation timestamp.
    fn event(
        state    : &mut Self,
        _proxy   : &ExtImageCopyCaptureFrameV1,
        event    : FrameEvent,
        _data    : &(),
        _conn    : &Connection,
        _qh      : &QueueHandle<Self>,
    ) {
        match event {
            FrameEvent::Transform { transform: WEnum::Value(t) } => {
                state.frame.transform = t;
            }

            FrameEvent::PresentationTime { tv_sec_hi, tv_sec_lo, tv_nsec } => {
                let secs = (u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo);
                state.frame.t_s = Some(secs as f64 + f64::from(tv_nsec) * 1e-9);
            }

            FrameEvent::Ready => {
                state.frame.done = true;
            }

            FrameEvent::Failed { reason } => {
                state.frame.failure = Some(match reason {
                    WEnum::Value(r) => r,
                    WEnum::Unknown(_) => FailureReason::Unknown,
                });
                state.frame.done = true;
            }

            // damage is not tracked: every capture damages the whole buffer.
            _ => {}
        }
    }
}

delegate_noop!(State: ignore WlShm);
delegate_noop!(State: ignore WlShmPool);
delegate_noop!(State: ignore WlBuffer);
delegate_noop!(State: ignore ExtImageCaptureSourceV1);
delegate_noop!(State: ignore ExtOutputImageCaptureSourceManagerV1);
delegate_noop!(State: ignore ExtImageCopyCaptureManagerV1);
delegate_noop!(State: ignore ZxdgOutputManagerV1);

// --- Error ---

/// Everything that can go wrong between connecting and holding pixels.
#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("cannot connect to the wayland compositor: {detail}")]
    Connect { detail: String },

    #[error("compositor does not implement {interface}")]
    MissingGlobal { interface: &'static str },

    #[error("wayland protocol error: {detail}")]
    Protocol { detail: String },

    #[error("no output named {name}")]
    NoSuchOutput { name: String },

    #[error("capture session for {output} stopped before delivering constraints")]
    SessionStopped { output: String },

    #[error("capture of {output} failed: {reason:?}")]
    FrameFailed { output: String, reason: FailureReason },

    #[error("output {output} has transform {transform}, only normal is supported")]
    UnsupportedTransform { output: String, transform: String },

    #[error("compositor offered no supported shm format, got {offered:?}")]
    NoUsableFormat { offered: Vec<Format> },

    #[error("invalid capture buffer size {len}")]
    BufferSize { len: usize },

    #[error("{what} failed: {detail}")]
    Shm { what: &'static str, detail: String },

    #[error("compositor did not respond within {ms} ms")]
    Timeout { ms: u64 },
}
