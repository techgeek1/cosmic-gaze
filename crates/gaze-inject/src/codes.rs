//! Shared evdev code mappings used by both backends.

use evdev::KeyCode;

use crate::Button;

/// High-resolution wheel units per traditional wheel "notch" (a `REL_WHEEL` value of 1).
/// Matches the kernel's `REL_WHEEL_HI_RES` convention that real mice have reported since
/// ~2019; sending both keeps the device correct for clients that only listen to one.
pub(crate) const WHEEL_HI_RES_PER_CLICK: i32 = 120;

/// Maps a `Button` to the `KeyCode` a real mouse reports for it.
pub(crate) fn button_code(button: Button) -> KeyCode {
    match button {
        Button::Left   => KeyCode::BTN_LEFT,
        Button::Right  => KeyCode::BTN_RIGHT,
        Button::Middle => KeyCode::BTN_MIDDLE,
    }
}
