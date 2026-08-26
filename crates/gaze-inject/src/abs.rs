//! The absolute backend: an `ABS_X`/`ABS_Y` uinput device scaled to the desk layout's
//! union bounding box. See the crate's module docs for the confirmed cosmic-comp
//! limitation this backend runs into (it does not honour a whole-layout absolute device
//! at all, and this capability set doesn't even get the correctly-mapped single-output
//! path) - this exists as the documented naive attempt, not the recommended backend.

use evdev::uinput::VirtualDevice;
use evdev::{
    AbsInfo, AbsoluteAxisCode, AbsoluteAxisEvent, AttributeSet, InputEvent, KeyCode, KeyEvent,
    PropType, RelativeAxisCode, RelativeAxisEvent, UinputAbsSetup,
};
use gaze_core::GlobalPx;

use crate::codes::{button_code, WHEEL_HI_RES_PER_CLICK};
use crate::layout::DeskLayout;
use crate::{Button, InjectBackend, InjectError, Result};

/// Name reported to the compositor/udev for the absolute device. Distinct from the
/// relative backend's name so both can coexist and so `udevadm info` or a settings panel
/// can tell which is which.
const DEVICE_NAME: &str = "gaze-inject (absolute)";

/// Top of the `ABS_X`/`ABS_Y` range, matching the digitiser convention most tablet and
/// touchscreen drivers use (`u16::MAX`) rather than the desk layout's raw pixel size, so
/// the axis resolution doesn't change if a monitor is added or resized.
const AXIS_MAX: i32 = u16::MAX as i32;

/// `ABS_X`/`ABS_Y` device: `INPUT_PROP_POINTER` (not `DIRECT`), ordinary mouse buttons,
/// no tool buttons. Per the crate's module docs, libinput classifies this as a plain
/// pointer, which cosmic-comp routes through a path that ignores `map_to_output` and
/// hardcodes `seat.active_output()` (pop-os/cosmic-comp#2103) - so `move_to`'s scaling
/// against the union bounding box only lands correctly when the target point falls inside
/// whichever output is currently active.
pub(crate) struct AbsoluteInjector {
    device : VirtualDevice,
    /// Union bounding box of every configured output, in global px: `(min_x, min_y,
    /// max_x, max_y)`. `move_to` scales into this box before converting to axis counts.
    bounds : (f64, f64, f64, f64),
}

// --- AbsoluteInjector ---

impl AbsoluteInjector {
    /// Opens `/dev/uinput` and registers the absolute device. Fails clearly (no panic) if
    /// the device node is missing, permission is denied despite the documented ACL, or any
    /// setup ioctl is rejected.
    pub(crate) fn create(layout: &DeskLayout) -> Result<AbsoluteInjector> {
        let abs_info = AbsInfo::new(0, 0, AXIS_MAX, 0, 0, 0);
        let abs_x    = UinputAbsSetup::new(AbsoluteAxisCode::ABS_X, abs_info);
        let abs_y    = UinputAbsSetup::new(AbsoluteAxisCode::ABS_Y, abs_info);

        let mut props = AttributeSet::<PropType>::new();
        props.insert(PropType::POINTER);

        let mut keys = AttributeSet::<KeyCode>::new();
        keys.insert(KeyCode::BTN_LEFT);
        keys.insert(KeyCode::BTN_RIGHT);
        keys.insert(KeyCode::BTN_MIDDLE);

        let mut rel_axes = AttributeSet::<RelativeAxisCode>::new();
        rel_axes.insert(RelativeAxisCode::REL_WHEEL);
        rel_axes.insert(RelativeAxisCode::REL_WHEEL_HI_RES);

        let device = VirtualDevice::builder()
            .map_err(InjectError::UinputOpen)?
            .name(DEVICE_NAME)
            .with_properties(&props)
            .map_err(InjectError::UinputSetup)?
            .with_absolute_axis(&abs_x)
            .map_err(InjectError::UinputSetup)?
            .with_absolute_axis(&abs_y)
            .map_err(InjectError::UinputSetup)?
            .with_keys(&keys)
            .map_err(InjectError::UinputSetup)?
            .with_relative_axes(&rel_axes)
            .map_err(InjectError::UinputSetup)?
            .build()
            .map_err(InjectError::UinputCreate)?;

        Ok(AbsoluteInjector { device: device, bounds: layout.bounds() })
    }
}

impl InjectBackend for AbsoluteInjector {
    fn move_to(&mut self, p: GlobalPx) -> Result<()> {
        let (x, y) = scale_to_abs(p, self.bounds);

        let events = [
            InputEvent::from(AbsoluteAxisEvent::new(AbsoluteAxisCode::ABS_X, x)),
            InputEvent::from(AbsoluteAxisEvent::new(AbsoluteAxisCode::ABS_Y, y)),
        ];

        self.device.emit(&events).map_err(InjectError::Emit)
    }

    fn click(&mut self, button: Button) -> Result<()> {
        let code = button_code(button);

        self.device.emit(&[InputEvent::from(KeyEvent::new(code, 1))]).map_err(InjectError::Emit)?;
        self.device.emit(&[InputEvent::from(KeyEvent::new(code, 0))]).map_err(InjectError::Emit)
    }

    fn scroll(&mut self, dy: i32) -> Result<()> {
        let events = [
            InputEvent::from(RelativeAxisEvent::new(RelativeAxisCode::REL_WHEEL, dy)),
            InputEvent::from(RelativeAxisEvent::new(
                RelativeAxisCode::REL_WHEEL_HI_RES,
                dy * WHEEL_HI_RES_PER_CLICK,
            )),
        ];

        self.device.emit(&events).map_err(InjectError::Emit)
    }
}

/// Maps a point in global px into the `[0, AXIS_MAX]` device range, clamping to `bounds`
/// first so a point just outside every output (an element bbox edge, or floating-point
/// slop) still lands inside the axis range instead of wrapping via the `as i32` cast.
fn scale_to_abs(p: GlobalPx, bounds: (f64, f64, f64, f64)) -> (i32, i32) {
    let (min_x, min_y, max_x, max_y) = bounds;

    // `.max(1.0)` guards a degenerate zero-width/height bounds tuple (a test fixture, in
    // practice) from a divide-by-zero; real desk layouts are always thousands of px wide.
    let w = (max_x - min_x).max(1.0);
    let h = (max_y - min_y).max(1.0);

    let x = (((p.x - min_x) / w).clamp(0.0, 1.0) * AXIS_MAX as f64).round() as i32;
    let y = (((p.y - min_y) / h).clamp(0.0, 1.0) * AXIS_MAX as f64).round() as i32;

    (x, y)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Union bounding box mirroring `config/desk.toml`: DP-2 at (0,160) 2560x1440, DP-1
    /// at (2559,0) 3840x1600, HDMI-A-1 at (1506,1600) 1670x1043 (fractional-scaled
    /// logical size, not the raw 1920x1200 physical panel). Max y comes from HDMI-A-1:
    /// 1600 + 1043 = 2643.
    fn desk_bounds() -> (f64, f64, f64, f64) {
        (0.0, 0.0, 6399.0, 2643.0)
    }

    #[test]
    fn origin_maps_to_zero() {
        let (x, y) = scale_to_abs(GlobalPx { x: 0.0, y: 0.0 }, desk_bounds());

        assert_eq!((x, y), (0, 0));
    }

    #[test]
    fn far_corner_maps_to_axis_max() {
        let (x, y) = scale_to_abs(GlobalPx { x: 6399.0, y: 2643.0 }, desk_bounds());

        assert_eq!((x, y), (AXIS_MAX, AXIS_MAX));
    }

    #[test]
    fn quarter_point_scales_proportionally() {
        // x = 6399 * 0.25 -> u = 0.25 exactly -> round(0.25 * 65535) = round(16383.75).
        let (x, _) = scale_to_abs(GlobalPx { x: 1599.75, y: 0.0 }, desk_bounds());

        assert_eq!(x, 16384);
    }

    #[test]
    fn three_quarter_point_scales_proportionally() {
        // y = 2643 * 0.75 -> u = 0.75 exactly -> round(0.75 * 65535) = round(49151.25).
        let (_, y) = scale_to_abs(GlobalPx { x: 0.0, y: 1982.25 }, desk_bounds());

        assert_eq!(y, 49151);
    }

    #[test]
    fn points_outside_bounds_clamp_into_range() {
        let (x, y) = scale_to_abs(GlobalPx { x: -100.0, y: 5000.0 }, desk_bounds());

        assert_eq!((x, y), (0, AXIS_MAX));
    }
}
