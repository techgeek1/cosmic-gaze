//! The key backend: a keyboard-shaped uinput device that carries the few keys the
//! session forwards on behalf of something else, today only `KEY_F13` for the voice
//! stack's push-to-talk.
//!
//! A separate device rather than extra keys on the mouse, on purpose: udev tags a device
//! by its capability set, and a mouse that also reports keyboard keys is classified as
//! both, which is a change to how the compositor treats the pointer device the
//! closed-loop backend was tuned against. A device with only `KEY_F13` is tagged
//! `ID_INPUT_KEY`, which libinput treats as a keyboard, and nothing else changes.
//!
//! The kernel releases every held key when a device is unregistered, so a session that
//! ends mid-hold cannot leave the key stuck once this is dropped.

use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, InputEvent, KeyCode, KeyEvent};

use crate::codes::key_code;
use crate::{InjectError, Key, Result};

/// Name reported to the compositor/udev for the key device.
const DEVICE_NAME: &str = "gaze-inject (keys)";

/// The keyboard-shaped device. Holds no state beyond the handle: press and release are
/// the caller's to pair.
pub(crate) struct KeyInjector {
    device : VirtualDevice,
}

// --- KeyInjector ---

impl KeyInjector {
    /// Opens `/dev/uinput` and registers a device carrying every [`Key`].
    pub(crate) fn create() -> Result<KeyInjector> {
        let mut keys = AttributeSet::<KeyCode>::new();

        for key in Key::ALL {
            keys.insert(key_code(key));
        }

        let device = VirtualDevice::builder()
            .map_err(InjectError::UinputOpen)?
            .name(DEVICE_NAME)
            .with_keys(&keys)
            .map_err(InjectError::UinputSetup)?
            .build()
            .map_err(InjectError::UinputCreate)?;

        Ok(KeyInjector { device: device })
    }

    /// Emits one key edge: down when `down`, up otherwise.
    pub(crate) fn key(&mut self, key: Key, down: bool) -> Result<()> {
        let code  = key_code(key);
        let value = if down { 1 } else { 0 };

        self.device.emit(&[InputEvent::from(KeyEvent::new(code, value))]).map_err(InjectError::Emit)
    }
}
