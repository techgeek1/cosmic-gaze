//! The Wayland half of the overlay: one `zwlr_layer_shell_v1` overlay surface per output,
//! `wl_shm` buffers, and the calloop event loop that drives them.
//!
//! The overlay is deliberately invisible to input. Each surface is created with an empty
//! `wl_region` as its input region, set before the first commit, so the compositor never
//! considers it a pointer target and clicks land on whatever is underneath. Keyboard
//! interactivity is `none` and the exclusive zone is -1, which asks the compositor not to
//! reserve any space and to let the surface sit over layer shell panels as well as
//! windows.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, Region};
use smithay_client_toolkit::output::{OutputHandler, OutputInfo, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shm::slot::{Buffer, SlotPool};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    registry_handlers,
};
use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop::channel::{self, Sender};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::wl_output::{Transform, WlOutput};
use smithay_client_toolkit::reexports::client::protocol::wl_shm::Format;
use smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface;
use smithay_client_toolkit::reexports::client::{Connection, Proxy, QueueHandle};
use tiny_skia::PixmapMut;
use tracing::{debug, error, warn};

use gaze_core::Rect;

use crate::draw::{self, PixelBox};
use crate::error::OverlayError;
use crate::mapping::OutputMapping;
use crate::present::{PointerStyle, Presenter};
use crate::state::OverlayState;
use crate::theme::{Theme, ThemeWatch};

/// Layer shell namespace. Compositors show this in debug output and rules can key off it.
const NAMESPACE: &str = "gaze-overlay";

/// Initial `wl_shm` pool size. The pool grows itself as surfaces are added, so this only
/// has to be big enough to avoid a resize for the first small surface.
const INITIAL_POOL_BYTES: usize = 1 << 20;

/// How long the event loop blocks before looking at the stop flag again.
const TICK: Duration = Duration::from_millis(20);

/// How long to wait for a frame callback before drawing anyway. A surface that is not
/// visible never gets its callback, and a debug overlay that silently freezes is worse
/// than one that occasionally draws a frame the compositor throws away.
const FRAME_TIMEOUT: Duration = Duration::from_millis(200);

/// A connected overlay: the Wayland state plus the event loop that drives it.
///
/// Not `Send`. The Wayland connection and every surface belong to the thread that called
/// [`Overlay::connect`]; other threads talk to it through an [`OverlayHandle`].
pub struct Overlay {
    /// Wayland state, the data type the calloop event loop dispatches into.
    app        : App,
    /// Owns the wayland event source and the state channel source.
    event_loop : EventLoop<'static, App>,
    /// Kept so state can be pushed in from other threads after the loop is running.
    sender     : Sender<OverlayState>,
    /// Used to flush pending requests after each dispatch.
    conn       : Connection,
    /// Set by any [`OverlayHandle::stop`]. Checked alongside the flag `run_until` is
    /// given, so either can end the loop.
    stop       : Arc<AtomicBool>,
}

/// A cloneable, sendable way to drive an overlay running on another thread.
#[derive(Clone)]
pub struct OverlayHandle {
    /// Wakes the overlay's event loop and delivers the new state.
    sender : Sender<OverlayState>,
    /// Set to ask the overlay thread to finish.
    stop   : Arc<AtomicBool>,
}

// --- Overlay ---

impl Overlay {
    /// Connects to the compositor named by `WAYLAND_DISPLAY`, binds the globals it needs
    /// and creates a surface for every output that already exists.
    ///
    /// Returns an error when there is no compositor, or when it does not implement
    /// `zwlr_layer_shell_v1` or `wl_shm`. Outputs that appear later are picked up by the
    /// event loop, so an empty output list is not an error.
    pub fn connect() -> Result<Overlay, OverlayError> {
        Overlay::connect_styled(PointerStyle::default())
    }

    /// [`Overlay::connect`] with the pointer look's tunables.
    pub fn connect_styled(style: PointerStyle) -> Result<Overlay, OverlayError> {
        let conn                   = Connection::connect_to_env()?;
        let (globals, event_queue) = registry_queue_init(&conn)?;
        let qh                     = event_queue.handle();

        let compositor  = CompositorState::bind(&globals, &qh)?;
        let layer_shell = LayerShell::bind(&globals, &qh)?;
        let shm         = Shm::bind(&globals, &qh)?;
        let pool        = SlotPool::new(INITIAL_POOL_BYTES, &shm)?;
        let region      = Region::new(&compositor)?;

        let mut app = App {
            registry_state : RegistryState::new(&globals),
            output_state   : OutputState::new(&globals, &qh),
            compositor     : compositor,
            layer_shell    : layer_shell,
            shm            : shm,
            pool           : pool,
            empty_region   : region,
            qh             : qh,
            surfaces       : Vec::new(),
            state          : OverlayState::default(),
            presenter      : Presenter::new(Theme::cosmic(), style),
            theme_watch    : ThemeWatch::start(),
            clock          : Instant::now(),
        };

        match &app.theme_watch {
            Some(w) => debug!(configs = w.watching(), "watching the cosmic theme"),
            None    => debug!("cosmic theme not watchable, the look is fixed for this run"),
        }

        // Two round trips: the first delivers the output globals and their `zxdg_output`
        // information, which is what `new_output` needs to place a surface; the second
        // picks up the configure events for the surfaces created during the first.
        let mut event_queue = event_queue;
        event_queue.roundtrip(&mut app)?;
        event_queue.roundtrip(&mut app)?;

        let event_loop      = EventLoop::try_new()?;
        let (sender, source) = channel::channel::<OverlayState>();

        event_loop
            .handle()
            .insert_source(source, |event, _, app: &mut App| {
                if let channel::Event::Msg(state) = event {
                    app.set(state);
                }
            })
            .map_err(|e| OverlayError::EventLoop(e.to_string()))?;

        WaylandSource::new(conn.clone(), event_queue)
            .insert(event_loop.handle())
            .map_err(|e| OverlayError::EventLoop(e.to_string()))?;

        Ok(Overlay {
            app        : app,
            event_loop : event_loop,
            sender     : sender,
            conn       : conn,
            stop       : Arc::new(AtomicBool::new(false)),
        })
    }

    /// Starts an overlay on its own thread.
    ///
    /// The Wayland connection is created on that thread and never leaves it. The returned
    /// handle pushes state over a channel that wakes the event loop, and stops the thread
    /// when [`OverlayHandle::stop`] is called or when the last handle is dropped and the
    /// caller joins. Connection errors surface here rather than on the thread.
    pub fn spawn()
        -> Result<(OverlayHandle, std::thread::JoinHandle<()>), OverlayError>
    {
        Overlay::spawn_styled(PointerStyle::default())
    }

    /// [`Overlay::spawn`] with the pointer look's tunables.
    pub fn spawn_styled(style: PointerStyle)
        -> Result<(OverlayHandle, std::thread::JoinHandle<()>), OverlayError>
    {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();

        let join = std::thread::Builder::new()
            .name(NAMESPACE.to_string())
            .spawn(move || {
                let mut overlay = match Overlay::connect_styled(style) {
                    Ok(o)  => o,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };

                // Hand the caller its handle before blocking. If the receiver is already
                // gone there is nobody to drive us, so exit instead of spinning.
                if ready_tx.send(Ok(overlay.handle())).is_err() {
                    return;
                }

                let stop = overlay.stop.clone();

                overlay.run_until(&stop);
            })
            .map_err(OverlayError::Spawn)?;

        let handle = ready_rx
            .recv()
            .map_err(|_| OverlayError::EventLoop("overlay thread died during startup".into()))??;

        Ok((handle, join))
    }

    /// Replaces what the overlay shows. Takes effect on the next iteration of
    /// [`Overlay::run_until`], or immediately if called from within it.
    pub fn set(&mut self, state: OverlayState) {
        self.app.set(state);
    }

    /// A handle that other threads can use to push state into this overlay and to stop
    /// it. Only useful once [`Overlay::run_until`] is running, since that is what drains
    /// the channel.
    pub fn handle(&self) -> OverlayHandle {
        OverlayHandle {
            sender : self.sender.clone(),
            stop   : self.stop.clone(),
        }
    }

    /// Runs the event loop until `stop` is set, or until any [`OverlayHandle::stop`] is
    /// called.
    ///
    /// Errors from the event loop are logged and end the loop rather than propagating,
    /// because the usual caller is a thread whose only job is to keep drawing. The
    /// surfaces are torn down before returning so the overlay does not linger on screen.
    pub fn run_until(&mut self, stop: &AtomicBool) {
        while !stop.load(Ordering::Relaxed) && !self.stop.load(Ordering::Relaxed) {
            if let Err(e) = self.event_loop.dispatch(Some(TICK), &mut self.app) {
                error!("overlay event loop failed: {e}");
                break;
            }

            self.app.redraw_pending();

            if let Err(e) = self.conn.flush() {
                error!("overlay connection flush failed: {e}");
                break;
            }
        }

        // Dropping the layer surfaces destroys them; the flush pushes those requests out
        // before the caller has a chance to exit the process.
        self.app.surfaces.clear();
        let _ = self.conn.flush();
    }

    /// The outputs the overlay currently covers, in the order it discovered them. Handy
    /// for a CLI to print, for rendering a frame offline, and for sanity checking the
    /// logical rectangles against `cosmic-randr list`.
    pub fn outputs(&self) -> impl Iterator<Item = &OutputMapping> {
        self.app.surfaces.iter().map(|s| &s.mapping)
    }

    /// The union of every output's logical rectangle, or `None` when there are no
    /// outputs. This is the area a caller can meaningfully place a gaze point in.
    pub fn desktop_bounds(&self) -> Option<Rect> {
        self.app.surfaces.iter().map(|s| s.mapping.logical).reduce(union_rect)
    }
}

// --- OverlayHandle ---

impl OverlayHandle {
    /// Pushes new state to the overlay thread and wakes its event loop.
    ///
    /// Returns an error only when the overlay thread has already exited, which the caller
    /// is usually happy to ignore during shutdown.
    pub fn set(&self, state: OverlayState) -> Result<(), OverlayError> {
        self.sender
            .send(state)
            .map_err(|_| OverlayError::EventLoop("overlay thread is gone".into()))
    }

    /// Asks the overlay thread to finish. The thread notices within one event loop tick;
    /// join its handle to wait for the surfaces to be destroyed.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

// --- App ---

/// Wayland state. This is the data type calloop dispatches into, so every handler
/// implementation below takes it as `&mut self`.
struct App {
    registry_state : RegistryState,
    output_state   : OutputState,
    compositor     : CompositorState,
    layer_shell    : LayerShell,
    shm            : Shm,
    /// One pool backs every surface. Slots are per surface and per buffer.
    pool           : SlotPool,
    /// A region with nothing added to it, shared by every surface as its input region.
    /// Kept alive for the lifetime of the overlay so the object is never destroyed while
    /// a surface still refers to it.
    empty_region   : Region,
    /// Needed to create surfaces and request frame callbacks outside a handler.
    qh             : QueueHandle<App>,
    surfaces       : Vec<OutputSurface>,
    state          : OverlayState,
    /// Animates the pointer look from the intents in `state`.
    presenter      : Presenter,
    /// Reloads the theme when cosmic-settings writes it. `None` without a COSMIC
    /// config directory to watch.
    theme_watch    : Option<ThemeWatch>,
    /// The presenter's timeline starts when the overlay does.
    clock          : Instant,
}

/// One output's layer surface and the buffers behind it.
struct OutputSurface {
    /// The output this surface is pinned to.
    output     : WlOutput,
    layer      : LayerSurface,
    mapping    : OutputMapping,
    /// Surface size in logical pixels from the last configure. `None` until then, and
    /// nothing may be drawn before then.
    size       : Option<(u32, u32)>,
    /// The two `wl_shm` buffers and which one to draw into next. `None` until the first
    /// configure, and dropped whenever the size changes.
    buffers    : Option<Buffers>,
    /// Set when the state changed since this surface was last drawn.
    dirty      : bool,
    /// When a frame callback was requested and has not arrived yet.
    frame_sent : Option<Instant>,
    /// Bounding box of what the last committed buffer drew, in buffer pixels: the marks
    /// the compositor is showing right now. Damage has to cover this as well as the new
    /// content, whichever buffer the new content lands in.
    on_screen  : PixelBox,
    /// The background the last committed buffer carried, for the same reason.
    on_screen_bg : Option<[u8; 4]>,
}

/// Double buffering for one surface. Drawing always targets the buffer the compositor is
/// not reading, so a frame is never modified while it is on screen.
struct Buffers {
    /// Buffer size in physical pixels, to detect a configure that changed it.
    size  : (u32, u32),
    slots : [Slot; 2],
    /// Index of the buffer to draw into next.
    next  : usize,
}

/// One buffer plus what is currently in it.
struct Slot {
    buffer     : Buffer,
    /// Bounding box of the pixels last drawn into this buffer, in buffer pixels. The next
    /// repaint has to cover this as well as the new content, or the old marker stays.
    content    : PixelBox,
    /// The background this buffer's untouched pixels currently hold. Every pixel outside
    /// `content` is exactly this colour, so a state whose background differs has to
    /// repaint the whole surface once; after that the partial repaints are correct again.
    background : Option<[u8; 4]>,
}

impl App {
    /// Replaces the overlay state and marks every surface for redraw.
    fn set(&mut self, state: OverlayState) {
        if self.state == state {
            return;
        }

        self.presenter.observe(state.pointer.as_ref());
        self.state = state;

        for surface in &mut self.surfaces {
            surface.dirty = true;
        }
    }

    /// Seconds since the overlay started: the presenter's clock.
    fn seconds(&self) -> f64 {
        self.clock.elapsed().as_secs_f64()
    }

    /// Draws every surface that has pending work and is allowed to draw now.
    fn redraw_pending(&mut self) {
        let now = Instant::now();

        // A theme change repaints everything in the new colours on the next frame.
        if self.theme_watch.as_ref().is_some_and(ThemeWatch::take_changed) {
            self.presenter.set_theme(Theme::cosmic());

            for surface in &mut self.surfaces {
                surface.dirty = true;
            }
        }

        for i in 0..self.surfaces.len() {
            let ready = {
                let s = &self.surfaces[i];

                s.dirty
                    && match s.frame_sent {
                        None    => true,
                        Some(t) => now.duration_since(t) > FRAME_TIMEOUT,
                    }
            };

            if ready {
                self.draw(i);
            }
        }
    }

    /// Repaints surface `index` and commits it.
    ///
    /// Only the union of what the target buffer already held and what the new state needs
    /// is cleared, redrawn and damaged. If both buffers are still held by the compositor
    /// the surface stays dirty and the next frame callback tries again.
    fn draw(&mut self, index: usize) {
        let Some((logical_w, logical_h)) = self.surfaces[index].size else {
            return;
        };

        let scale  = self.surfaces[index].mapping.scale as u32;
        let width  = logical_w * scale;
        let height = logical_h * scale;

        if width == 0 || height == 0 {
            return;
        }

        // Reallocate on the first draw and after any size change.
        let stale = match &self.surfaces[index].buffers {
            Some(b) => b.size != (width, height),
            None    => true,
        };

        if stale {
            self.surfaces[index].buffers = None;

            match allocate(&mut self.pool, width, height) {
                Ok(b)  => self.surfaces[index].buffers = Some(b),
                Err(e) => {
                    error!(
                        output = %self.surfaces[index].mapping.name,
                        "overlay buffer allocation failed: {e}",
                    );
                    self.surfaces[index].dirty = false;
                    return;
                }
            }
        }

        // The presenter is stepped per draw rather than per tick so a surface drawn
        // from its frame callback sees the fades as of now, not of the last tick.
        let animating = self.presenter.step(self.seconds());
        let mut items = draw::scene(&self.state, &self.surfaces[index].mapping);

        items.extend(self.presenter.scene(&self.surfaces[index].mapping));

        let wanted     = draw::bounds(&items).unwrap_or(PixelBox::EMPTY).clip_to(width, height);
        let background = self.state.background;
        let whole      = PixelBox { x: 0, y: 0, w: width as i32, h: height as i32 };

        // Prefer the buffer that is not on screen. If it is somehow still busy the other
        // one will do; if both are, stay dirty and wait for a release.
        let buffers = self.surfaces[index].buffers.as_ref().expect("allocated above");

        let Some(slot_index) = pick_free(&mut self.pool, buffers)
        else {
            debug!(
                output = %self.surfaces[index].mapping.name,
                "both overlay buffers are held by the compositor, deferring",
            );
            return;
        };

        // Two different rectangles. The *repaint* is what has to be redrawn in this
        // buffer: the union of what it already held and what it needs to hold, because
        // anything outside that is still valid from the frame this buffer last showed.
        // The *damage* is what the compositor has to re-read: the union of what is on
        // screen now and the new content. They differ under double buffering, and using
        // the repaint as the damage was a real bug: a blank frame drawn into the buffer
        // that was already blank repainted nothing, so it was committed with no damage,
        // and the compositor kept showing the other buffer's box and cross. The click
        // probe then captured its own marks and the widget model called them a button.
        let damage_area = damage_for(
            self.surfaces[index].on_screen,
            wanted,
            self.surfaces[index].on_screen_bg != background,
            whole,
        );

        {
            let slot    = &mut self.surfaces[index].buffers.as_mut().unwrap().slots[slot_index];
            let repaint = {
                if slot.background == background {
                    slot.content.union(wanted)
                }
                else {
                    whole
                }
            };

            let Some(canvas) = self.pool.canvas(&slot.buffer) else {
                return;
            };

            let Some(mut pixmap) = PixmapMut::from_bytes(canvas, width, height) else {
                error!("overlay buffer is not a valid {width}x{height} pixmap");
                return;
            };

            if !repaint.is_empty() {
                draw::clear(&mut pixmap, repaint, background);
                draw::draw(&mut pixmap, &items);
                draw::rgba_to_argb(&mut pixmap, repaint);
            }

            slot.content    = wanted;
            slot.background = background;
        }

        let surface = &mut self.surfaces[index];

        debug!(
            output    = %surface.mapping.name,
            blank     = self.state.is_blank(),
            damage    = ?damage_area,
            waited_ms = surface.frame_sent.map(|t| t.elapsed().as_millis()),
            "overlay commit"
        );

        // While a fade is in progress the surface stays dirty, so the frame callback
        // just requested draws the next step; the animation is paced by the compositor
        // and stops asking for frames the moment it settles.
        surface.buffers.as_mut().unwrap().next = 1 - slot_index;
        surface.dirty                          = animating;
        surface.frame_sent                     = Some(Instant::now());
        surface.on_screen                      = wanted;
        surface.on_screen_bg                   = background;

        let scale      = surface.mapping.scale;
        let wl_surface = surface.layer.wl_surface();

        if !damage_area.is_empty() {
            damage(wl_surface, damage_area, scale);
        }

        // Ask for a frame callback so the next update is paced by the compositor rather
        // than by however fast the producer thread pushes state.
        wl_surface.frame(&self.qh, wl_surface.clone());

        let buffer = &surface.buffers.as_ref().unwrap().slots[slot_index].buffer;

        if let Err(e) = buffer.attach_to(wl_surface) {
            error!("overlay buffer attach failed: {e}");
            return;
        }

        wl_surface.commit();
    }

    /// Creates a layer surface for an output, or updates the existing one when the output
    /// moved, resized or changed scale.
    fn sync_output(&mut self, output: &WlOutput) {
        let Some(info) = self.output_state.info(output) else {
            return;
        };

        let Some(mapping) = mapping_of(&info) else {
            debug!("output {:?} has no usable logical geometry yet", info.name);
            return;
        };

        if let Some(index) = self.surfaces.iter().position(|s| &s.output == output) {
            let surface = &mut self.surfaces[index];

            if surface.mapping == mapping {
                return;
            }

            debug!(
                output = %mapping.name,
                "overlay output geometry changed to {:?} scale {}",
                mapping.logical, mapping.scale,
            );

            if surface.mapping.scale != mapping.scale {
                let _ = surface.layer.set_buffer_scale(mapping.scale as u32);
            }

            surface.mapping = mapping;
            surface.buffers = None;
            surface.dirty   = true;
            surface.layer.commit();
            return;
        }

        self.create_surface(output.clone(), mapping);
    }

    /// Builds one overlay surface. Everything that has to be part of the surface's
    /// initial state, the empty input region above all, is set before the first commit.
    fn create_surface(&mut self, output: WlOutput, mapping: OutputMapping) {
        let surface = self.compositor.create_surface(&self.qh);
        let layer   = self.layer_shell.create_layer_surface(
            &self.qh,
            surface,
            Layer::Overlay,
            Some(NAMESPACE),
            Some(&output),
        );

        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);

        // Zero means "as large as the anchor rectangle", which with all four anchors set
        // is the whole output.
        layer.set_size(0, 0);

        // The whole point of the crate: an empty input region makes every pointer event
        // pass straight through to whatever is underneath.
        layer.set_input_region(Some(self.empty_region.wl_region()));

        if layer.set_buffer_scale(mapping.scale as u32).is_err() {
            warn!(output = %mapping.name, "wl_surface is too old for set_buffer_scale");
        }

        // The initial commit carries no buffer; the compositor answers with a configure.
        layer.commit();

        debug!(
            output = %mapping.name,
            "overlay surface created for {:?} scale {}",
            mapping.logical, mapping.scale,
        );

        self.surfaces.push(OutputSurface {
            output     : output,
            layer      : layer,
            mapping    : mapping,
            size       : None,
            buffers    : None,
            dirty      : true,
            frame_sent : None,
            on_screen  : PixelBox::EMPTY,
            on_screen_bg : None,
        });
    }

    /// Index of the surface owning a `wl_surface`.
    fn surface_index(&self, surface: &WlSurface) -> Option<usize> {
        self.surfaces.iter().position(|s| s.layer.wl_surface() == surface)
    }
}

// --- Handlers ---

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _conn      : &Connection,
        _qh        : &QueueHandle<Self>,
        _surface   : &WlSurface,
        _new_factor: i32,
    ) {
        // The output's own scale drives our buffers; `update_output` handles a change.
    }

    fn transform_changed(
        &mut self,
        _conn      : &Connection,
        _qh        : &QueueHandle<Self>,
        _surface   : &WlSurface,
        _transform : Transform,
    ) {
        // Rotated outputs are out of scope for phase 0.
    }

    fn frame(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, surface: &WlSurface, _t: u32) {
        let Some(index) = self.surface_index(surface) else {
            return;
        };

        debug!(
            output = %self.surfaces[index].mapping.name,
            after_ms = self.surfaces[index].frame_sent.map(|t| t.elapsed().as_millis()),
            "overlay frame callback"
        );

        self.surfaces[index].frame_sent = None;

        if self.surfaces[index].dirty {
            self.draw(index);
        }
    }

    fn surface_enter(
        &mut self,
        _conn    : &Connection,
        _qh      : &QueueHandle<Self>,
        _surface : &WlSurface,
        _output  : &WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn    : &Connection,
        _qh      : &QueueHandle<Self>,
        _surface : &WlSurface,
        _output  : &WlOutput,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, output: WlOutput) {
        self.sync_output(&output);
    }

    fn update_output(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, output: WlOutput) {
        self.sync_output(&output);
    }

    fn output_destroyed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, output: WlOutput) {
        // Dropping the surface destroys the layer surface role and then the wl_surface,
        // in that order, which is what the layer shell protocol requires.
        if let Some(index) = self.surfaces.iter().position(|s| s.output == output) {
            debug!(output = %self.surfaces[index].mapping.name, "overlay output went away");
            self.surfaces.remove(index);
        }
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, layer: &LayerSurface) {
        if let Some(index) = self.surfaces.iter().position(|s| &s.layer == layer) {
            warn!(output = %self.surfaces[index].mapping.name, "overlay surface was closed");
            self.surfaces.remove(index);
        }
    }

    fn configure(
        &mut self,
        _conn     : &Connection,
        _qh       : &QueueHandle<Self>,
        layer     : &LayerSurface,
        configure : LayerSurfaceConfigure,
        _serial   : u32,
    ) {
        let Some(index) = self.surfaces.iter().position(|s| &s.layer == layer) else {
            return;
        };

        let surface = &mut self.surfaces[index];

        // A zero dimension means the compositor left the choice to us; with all four
        // anchors set that only happens if it does not know the output size, so fall
        // back to what xdg-output told us.
        let (w, h) = configure.new_size;
        let size   = (
            if w == 0 { surface.mapping.logical.w as u32 } else { w },
            if h == 0 { surface.mapping.logical.h as u32 } else { h },
        );

        surface.size  = Some(size);
        surface.dirty = true;

        self.draw(index);
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState];
}

delegate_compositor!(App);
delegate_output!(App);
delegate_shm!(App);
delegate_layer!(App);
delegate_registry!(App);

// --- Internals ---

/// Builds a mapping from what the compositor said about an output.
///
/// `zxdg_output_v1` is authoritative for position and size: cosmic-comp drives HDMI-A-1
/// at a fractional scale and still advertises `wl_output.scale` 2, so trusting the
/// integer scale to convert the mode into a logical size puts the surface origin and
/// every marker in the wrong place.
///
/// Returns `None` while the information is still incomplete, which happens between the
/// `wl_output` global appearing and its `zxdg_output_v1` events arriving. The caller
/// simply tries again on the next `update_output`.
fn mapping_of(info: &OutputInfo) -> Option<OutputMapping> {
    let name   = info.name.clone()?;
    let (x, y) = info.logical_position.unwrap_or(info.location);

    // Without xdg-output there is nothing better than the mode divided by the advertised
    // scale, which is what the compositor would have computed for an integer scale.
    let (w, h) = {
        if let Some(size) = info.logical_size {
            size
        }
        else {
            let mode  = info.modes.iter().find(|m| m.current)?;
            let scale = info.scale_factor.max(1);

            (mode.dimensions.0 / scale, mode.dimensions.1 / scale)
        }
    };

    if w <= 0 || h <= 0 {
        return None;
    }

    Some(OutputMapping::new(
        name,
        Rect { x: f64::from(x), y: f64::from(y), w: f64::from(w), h: f64::from(h) },
        buffer_scale(info, w),
    ))
}

/// Chooses the integer buffer scale for an output whose logical width is `logical_w`.
///
/// The honest scale is the ratio between the current mode's physical width and the
/// logical width, which is 1.15 on the fractionally scaled HDMI-A-1 and exactly 2 on a
/// real HiDPI panel. Rounding it gives 1 for the former, so that surface is rendered at
/// logical size and the compositor scales it up: a debug overlay of rings and boxes can
/// afford to be slightly soft, and the alternative costs 20 MB of shared memory per
/// buffer on that panel alone. `wl_output.scale` is only a fallback, since cosmic-comp
/// reports 2 for an output that is nothing of the sort.
fn buffer_scale(info: &OutputInfo, logical_w: i32) -> i32 {
    let Some(mode) = info.modes.iter().find(|m| m.current)
    else {
        return info.scale_factor.max(1);
    };

    if logical_w <= 0 || mode.dimensions.0 <= 0 {
        return info.scale_factor.max(1);
    }

    let ratio = f64::from(mode.dimensions.0) / f64::from(logical_w);

    (ratio.round() as i32).clamp(1, 4)
}

/// Allocates a fresh pair of buffers of the given physical size.
fn allocate(pool: &mut SlotPool, width: u32, height: u32) -> Result<Buffers, OverlayError> {
    let stride = width as i32 * 4;
    let mut make = || -> Result<Slot, OverlayError> {
        let slot   = pool.new_slot(height as usize * stride as usize)?;
        let buffer = pool.create_buffer_in(
            &slot,
            width as i32,
            height as i32,
            stride,
            Format::Argb8888,
        )?;

        Ok(Slot {
            buffer     : buffer,
            content    : PixelBox::EMPTY,
            background : None,
        })
    };

    Ok(Buffers {
        size  : (width, height),
        slots : [make()?, make()?],
        next  : 0,
    })
}

/// Picks a buffer the compositor is not currently reading, preferring the one that is
/// next in rotation. Returns `None` when both are in use.
fn pick_free(pool: &mut SlotPool, buffers: &Buffers) -> Option<usize> {
    let order = [buffers.next, 1 - buffers.next];

    order.into_iter().find(|&i| pool.canvas(&buffers.slots[i].buffer).is_some())
}

/// The rectangle the compositor has to re-read after a commit: whatever it is showing
/// now plus whatever the new frame draws, or the whole surface when the background
/// changed underneath everything. Empty only when nothing was and nothing will be shown.
fn damage_for(
    on_screen          : PixelBox,
    wanted             : PixelBox,
    background_changed : bool,
    whole              : PixelBox,
)
    -> PixelBox
{
    if background_changed {
        return whole;
    }

    on_screen.union(wanted)
}

/// Reports a repainted rectangle to the compositor. `damage_buffer` takes buffer pixels
/// and is what we want; the pre-version-4 `damage` request takes surface-local logical
/// pixels, so the rectangle has to be divided by the scale (and grown to be safe).
fn damage(surface: &WlSurface, area: PixelBox, scale: i32) {
    if surface.version() >= 4 {
        surface.damage_buffer(area.x, area.y, area.w, area.h);
        return;
    }

    surface.damage(
        area.x / scale,
        area.y / scale,
        area.w / scale + 1,
        area.h / scale + 1,
    );
}

/// Smallest rectangle covering both, in global logical pixels.
fn union_rect(a: Rect, b: Rect) -> Rect {
    let x0 = a.x.min(b.x);
    let y0 = a.y.min(b.y);
    let x1 = (a.x + a.w).max(b.x + b.w);
    let y1 = (a.y + a.h).max(b.y + b.h);

    Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// The double-buffering case that bit: a blank frame after a box must damage the box,
    /// even though the buffer it lands in was already blank.
    #[test]
    fn a_blank_frame_damages_what_the_other_buffer_showed() {
        let whole = PixelBox { x: 0, y: 0, w: 1000, h: 800 };
        let boxed = PixelBox { x: 100, y: 100, w: 50, h: 40 };

        assert_eq!(damage_for(boxed, PixelBox::EMPTY, false, whole), boxed);
        assert_eq!(damage_for(PixelBox::EMPTY, boxed, false, whole), boxed);
        assert!(damage_for(PixelBox::EMPTY, PixelBox::EMPTY, false, whole).is_empty());
        assert_eq!(damage_for(PixelBox::EMPTY, PixelBox::EMPTY, true, whole), whole);
    }

    #[test]
    fn union_of_the_desk_layout_covers_every_output() {
        let dp1  = Rect { x: 2559.0, y: 0.0, w: 3840.0, h: 1600.0 };
        let dp2  = Rect { x: 0.0, y: 160.0, w: 2560.0, h: 1440.0 };
        let hdmi = Rect { x: 1506.0, y: 1600.0, w: 1670.0, h: 1043.0 };

        // Matches what `gaze-overlay-cli --list` prints against the live compositor.
        let all = union_rect(union_rect(dp1, dp2), hdmi);

        assert_eq!(all, Rect { x: 0.0, y: 0.0, w: 6399.0, h: 2643.0 });
    }
}
