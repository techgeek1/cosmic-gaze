//! Finding and opening the evdev device to grab as the gaze mouse.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use evdev::Device;

/// Directory evdev device nodes live under.
const DEV_INPUT_DIR: &str = "/dev/input";

/// Finds and opens the evdev device to grab: `path` if given, otherwise the first device
/// under `/dev/input` whose kernel-reported name contains `name_substr`.
///
/// Name lookup reads `/sys/class/input/eventN/device/name` rather than opening every
/// `/dev/input/eventN` node in turn, so a device can be located by name even when the
/// caller lacks permission to open nodes it isn't targeting -- only the chosen device
/// needs to open successfully. Returns the resolved path alongside the open device so
/// callers can report it in later error messages (e.g. a failed grab).
pub fn find_device(path: Option<&Path>, name_substr: &str)
    -> Result<(PathBuf, Device), DeviceError>
{
    let chosen = match path {
        Some(p) => p.to_path_buf(),
        None    => find_by_name(name_substr)?,
    };

    let device = open(&chosen)?;

    Ok((chosen, device))
}

/// Scans `/dev/input/eventN` nodes in numeric order and returns the path of the first one
/// whose sysfs name contains `name_substr`.
fn find_by_name(name_substr: &str) -> Result<PathBuf, DeviceError> {
    let mut candidates: Vec<(u32, PathBuf)> = fs::read_dir(DEV_INPUT_DIR)
        .map_err(|source| DeviceError::ScanDir { dir: PathBuf::from(DEV_INPUT_DIR), source })?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let path = entry.path();
            let n = path.file_name()?.to_str()?.strip_prefix("event")?.parse::<u32>().ok()?;

            Some((n, path))
        })
        .collect();

    // Numeric order keeps device selection deterministic across runs; readdir order isn't
    // guaranteed to be stable.
    candidates.sort_by_key(|(n, _)| *n);

    for (n, path) in candidates {
        let sysfs_name = PathBuf::from(format!("/sys/class/input/event{n}/device/name"));

        let Ok(name) = fs::read_to_string(&sysfs_name) else {
            continue;
        };

        if name.trim().contains(name_substr) {
            return Ok(path);
        }
    }

    Err(DeviceError::NoMatch { name_substr: name_substr.to_string() })
}

/// Opens an evdev device node, translating a permission error into actionable guidance.
/// `/dev/input/event*` is group `input` by default and a fresh user account is often not
/// a member yet, so this is the common first failure and deserves a message that fixes
/// itself rather than a bare `EACCES`.
fn open(path: &Path) -> Result<Device, DeviceError> {
    Device::open(path).map_err(|source| {
        if source.kind() == ErrorKind::PermissionDenied {
            DeviceError::PermissionDenied { path: path.to_path_buf() }
        }
        else {
            DeviceError::Open { path: path.to_path_buf(), source }
        }
    })
}

/// Hi-res wheel units the kernel reports per physical detent. `REL_WHEEL_HI_RES` is
/// defined in multiples of 120 (`include/linux/input.h`), so a normal notch on a
/// non-freewheeling mouse is exactly one detent.
const HI_RES_PER_DETENT: i32 = 120;

/// Turns a device's raw wheel reports into whole detents.
///
/// A mouse that supports high-resolution scrolling emits both `REL_WHEEL` and
/// `REL_WHEEL_HI_RES` for the same physical motion, so naively summing them double counts
/// every notch. This prefers the hi-res axis once it has seen one, keeps the sub-detent
/// remainder across batches so a slow freewheel eventually produces a detent, and falls
/// back to plain `REL_WHEEL` on devices that only report that.
///
/// One accumulator belongs to one device. Feed it every wheel event in an evdev read
/// batch, then call [`take`](Self::take) once at the end of the batch: the axis preference
/// is resolved at `take` time, so it does not matter which of the two axes the kernel
/// happens to report first.
#[derive(Clone, Copy, Debug, Default)]
pub struct WheelAccumulator {
    /// Sticky: once this device has reported the hi-res axis, the plain axis is ignored
    /// for the rest of the session.
    hi_res_seen : bool,
    /// Hi-res units seen but not yet worth a whole detent. Signed, same sign as the
    /// motion, so reversing direction cancels rather than accumulating.
    residual    : i32,
    /// Whole detents from the hi-res axis, pending a `take`.
    hi_detents  : i32,
    /// Whole detents from the plain axis, pending a `take`. Discarded the moment the
    /// hi-res axis shows up.
    notches     : i32,
}

// --- WheelAccumulator ---

impl WheelAccumulator {
    /// A fresh accumulator that has not yet decided which wheel axis this device uses.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one `REL_WHEEL_HI_RES` value, in 1/120ths of a detent.
    pub fn hi_res(&mut self, value: i32) {
        // The plain-axis tally is dropped rather than kept, because anything already in it
        // describes the same physical motion this hi-res report describes.
        if !self.hi_res_seen {
            self.hi_res_seen = true;
            self.notches     = 0;
        }

        self.residual += value;

        // Truncating division keeps the remainder's sign, so a downward freewheel doesn't
        // round its way into an upward detent.
        let whole = self.residual / HI_RES_PER_DETENT;

        self.hi_detents += whole;
        self.residual   -= whole * HI_RES_PER_DETENT;
    }

    /// Feeds one `REL_WHEEL` value, in whole detents. Ignored on a device that also
    /// reports the hi-res axis.
    pub fn notch(&mut self, value: i32) {
        if !self.hi_res_seen {
            self.notches += value;
        }
    }

    /// Takes the whole detents accumulated since the last call. Positive is up, away from
    /// the user, matching the kernel's convention and `gaze_inject::Injector::scroll`.
    pub fn take(&mut self) -> i32 {
        let out = {
            if self.hi_res_seen {
                self.hi_detents
            }
            else {
                self.notches
            }
        };

        self.hi_detents = 0;
        self.notches    = 0;

        out
    }
}

// --- Error ---

#[derive(Debug, thiserror::Error)]
pub enum DeviceError {
    #[error("cannot scan {} for evdev devices: {source}", dir.display())]
    ScanDir { dir: PathBuf, #[source] source: std::io::Error },

    #[error("no evdev device found with name containing {name_substr:?}")]
    NoMatch { name_substr: String },

    #[error(
        "cannot open {}: permission denied; add your user to the `input` group or run: \
         sudo setfacl -m u:$USER:rw /dev/input/event*",
        path.display()
    )]
    PermissionDenied { path: PathBuf },

    #[error("cannot open {}: {source}", path.display())]
    Open { path: PathBuf, #[source] source: std::io::Error },
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn open_permission_denied_maps_to_an_actionable_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fake-event0");
        fs::write(&path, b"").unwrap();
        fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();

        let err = open(&path).unwrap_err();

        assert!(matches!(err, DeviceError::PermissionDenied { .. }));

        let message = err.to_string();
        assert!(message.contains("permission denied"));
        assert!(message.contains("setfacl"));
        assert!(message.contains(&path.display().to_string()));
    }

    #[test]
    fn open_missing_path_is_a_generic_open_error_not_a_permission_error() {
        let err = open(Path::new("/nonexistent/definitely-not-a-real-path")).unwrap_err();

        assert!(matches!(err, DeviceError::Open { .. }));
    }

    #[test]
    fn find_device_with_an_explicit_path_skips_name_lookup() {
        // A bogus name substring would fail `find_by_name`; passing an explicit path
        // must never consult it.
        let err = find_device(Some(Path::new("/nonexistent/definitely-not-a-real-path")), "this substring is never looked up")
            .unwrap_err();

        assert!(matches!(err, DeviceError::Open { .. }));
    }

    #[test]
    fn wheel_accumulator_counts_plain_notches_on_a_low_res_mouse() {
        let mut wheel = WheelAccumulator::new();

        wheel.notch(1);
        wheel.notch(1);
        assert_eq!(wheel.take(), 2);

        wheel.notch(-3);
        assert_eq!(wheel.take(), -3);

        // Nothing new since the last take.
        assert_eq!(wheel.take(), 0);
    }

    /// The double-count trap: a hi-res mouse reports the same physical notch on both axes,
    /// in either order, and only one of them may be counted.
    #[test]
    fn wheel_accumulator_ignores_the_plain_axis_once_hi_res_shows_up() {
        for plain_first in [true, false] {
            let mut wheel = WheelAccumulator::new();

            if plain_first {
                wheel.notch(1);
                wheel.hi_res(120);
            }
            else {
                wheel.hi_res(120);
                wheel.notch(1);
            }

            assert_eq!(wheel.take(), 1, "plain_first = {plain_first}");
        }
    }

    #[test]
    fn wheel_accumulator_holds_sub_detent_hi_res_motion_across_batches() {
        let mut wheel = WheelAccumulator::new();

        for _ in 0..3 {
            wheel.hi_res(30);
            assert_eq!(wheel.take(), 0);
        }

        wheel.hi_res(30);
        assert_eq!(wheel.take(), 1);
    }

    /// A freewheel that reverses direction must cancel, not ratchet: the remainder keeps
    /// the sign of the motion that produced it.
    #[test]
    fn wheel_accumulator_cancels_reversed_sub_detent_motion() {
        let mut wheel = WheelAccumulator::new();

        wheel.hi_res(90);
        wheel.hi_res(-90);
        assert_eq!(wheel.take(), 0);

        wheel.hi_res(-120);
        assert_eq!(wheel.take(), -1);
    }
}
