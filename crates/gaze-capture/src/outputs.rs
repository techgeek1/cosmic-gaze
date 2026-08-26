//! Output enumeration shared by the frame-capture and cursor-tracking connections.
//!
//! Both live on their own `wl_display`, so neither can borrow the other's proxies, but the
//! bookkeeping is identical: bind every `wl_output`, ask `zxdg_output_manager_v1` for its
//! logical rectangle, and drop the entry when the global goes away. The per-connection
//! `Dispatch` impls are the only part that has to be written twice, and they do nothing
//! but forward into `apply_wl_output` and `apply_xdg_output`.

use gaze_core::{GlobalPx, Rect};
use wayland_client::protocol::wl_output::{Event as WlOutputEvent, Mode, WlOutput};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::{Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_manager_v1::ZxdgOutputManagerV1;
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_v1::{
    Event as XdgOutputEvent, ZxdgOutputV1,
};

use crate::frame::OutputInfo;

/// `wl_output` version to bind. Version 4 is what cosmic-comp advertises and is the first
/// with the `name` event, which is where the connector string comes from.
pub(crate) const WL_OUTPUT_VERSION: u32 = 4;

/// `zxdg_output_manager_v1` version to bind. Version 2 and up send `logical_position` and
/// `logical_size` without needing a `wl_output.done` to commit them, and version 3 is what
/// cosmic-comp advertises.
pub(crate) const XDG_OUTPUT_VERSION: u32 = 3;

/// One `wl_output` plus the properties needed to describe and capture it.
///
/// Geometry fields are `Option` because the compositor sends them asynchronously; an entry
/// with any of them missing is not yet usable and `info` returns `None` for it.
pub(crate) struct OutputEntry {
    /// Registry name, used to match `global_remove` and to route output events.
    pub(crate) global_name : u32,
    pub(crate) wl_output   : WlOutput,
    pub(crate) xdg_output  : Option<ZxdgOutputV1>,
    /// Connector name from `wl_output.name`, falling back to `zxdg_output_v1.name`.
    pub(crate) name        : Option<String>,
    pub(crate) logical_x   : Option<i32>,
    pub(crate) logical_y   : Option<i32>,
    pub(crate) logical_w   : Option<i32>,
    pub(crate) logical_h   : Option<i32>,
    /// Current mode in physical pixels. Zero until the first `wl_output.mode`.
    pub(crate) mode_w      : i32,
    pub(crate) mode_h      : i32,
}

// --- OutputEntry ---

impl OutputEntry {
    /// Describes the output, or `None` while any property is still outstanding.
    pub(crate) fn info(&self) -> Option<OutputInfo> {
        let name = self.name.clone()?;
        let x    = self.logical_x?;
        let y    = self.logical_y?;
        let w    = self.logical_w?;
        let h    = self.logical_h?;

        if w <= 0 || h <= 0 || self.mode_w <= 0 || self.mode_h <= 0 {
            return None;
        }

        Some(OutputInfo {
            name       : name,
            logical    : Rect {
                x : f64::from(x),
                y : f64::from(y),
                w : f64::from(w),
                h : f64::from(h),
            },
            scale      : f64::from(self.mode_w) / f64::from(w),
            physical_w : self.mode_w as u32,
            physical_h : self.mode_h as u32,
        })
    }

    /// Maps a point in this output's capture-buffer pixels to global logical pixels.
    ///
    /// `None` while the output's geometry is still incomplete. See
    /// [`OutputInfo::buffer_to_global`] for the conversion itself.
    pub(crate) fn buffer_to_global(&self, x: f64, y: f64) -> Option<GlobalPx> {
        Some(self.info()?.buffer_to_global(x, y))
    }
}

// --- Shared registry and event handling ---

/// Binds a newly advertised `wl_output` and requests its xdg counterpart.
///
/// The caller owns the resulting entry and is responsible for pushing it into its own
/// output list. `udata` on both proxies is the registry name, which is how the `Dispatch`
/// impls find the entry again.
pub(crate) fn bind_output<D>(
    qh          : &QueueHandle<D>,
    registry    : &WlRegistry,
    xdg_mgr     : &ZxdgOutputManagerV1,
    global_name : u32,
    version     : u32,
)
    -> OutputEntry
where
    D: Dispatch<WlOutput, u32> + Dispatch<ZxdgOutputV1, u32> + 'static,
{
    let version   = version.min(WL_OUTPUT_VERSION);
    let wl_output : WlOutput = registry.bind(global_name, version, qh, global_name);
    let xdg       = xdg_mgr.get_xdg_output(&wl_output, qh, global_name);

    OutputEntry {
        global_name : global_name,
        wl_output   : wl_output,
        xdg_output  : Some(xdg),
        name        : None,
        logical_x   : None,
        logical_y   : None,
        logical_w   : None,
        logical_h   : None,
        mode_w      : 0,
        mode_h      : 0,
    }
}

/// True when the registry global describes a `wl_output`.
pub(crate) fn is_output(interface: &str) -> bool {
    interface == WlOutput::interface().name
}

/// Folds a `wl_output` event into an entry: connector name and current mode size.
pub(crate) fn apply_wl_output(entry: &mut OutputEntry, event: WlOutputEvent) {
    match event {
        WlOutputEvent::Name { name } => {
            entry.name = Some(name);
        }

        WlOutputEvent::Mode { flags, width, height, .. } => {
            // Only the current mode describes the capture buffer size.
            let current = matches!(flags, WEnum::Value(f) if f.contains(Mode::Current));

            if current {
                entry.mode_w = width;
                entry.mode_h = height;
            }
        }

        _ => {}
    }
}

/// Folds a `zxdg_output_v1` event into an entry: the logical rectangle, and the connector
/// name when `wl_output` did not supply one.
pub(crate) fn apply_xdg_output(entry: &mut OutputEntry, event: XdgOutputEvent) {
    match event {
        XdgOutputEvent::LogicalPosition { x, y } => {
            entry.logical_x = Some(x);
            entry.logical_y = Some(y);
        }

        XdgOutputEvent::LogicalSize { width, height } => {
            entry.logical_w = Some(width);
            entry.logical_h = Some(height);
        }

        // `wl_output.name` is authoritative when the compositor sent it; this is the
        // fallback for a version 3 output that has no name event of its own.
        XdgOutputEvent::Name { name } if entry.name.is_none() => {
            entry.name = Some(name);
        }

        _ => {}
    }
}

/// Tears down the proxies of an output the compositor has taken away.
pub(crate) fn release_output(entry: OutputEntry) {
    if let Some(xdg) = entry.xdg_output {
        xdg.destroy();
    }

    // `wl_output.release` only exists from version 3; sending it to an older object would
    // be a protocol error, and letting the proxy drop is harmless there.
    if entry.wl_output.version() >= 3 {
        entry.wl_output.release();
    }
}

