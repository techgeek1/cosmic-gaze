//! The daemon's control interface on the session bus.
//!
//! One object, `/dev/techgeek1/CosmicGaze`, on the well-known name
//! `dev.techgeek1.CosmicGaze`, with the interface of the same name. Properties say what
//! the session is doing ([`Status`]); three methods tell it to pause, resume, and forget
//! the day's offset. The daemon implements the interface; the applet, and anything
//! `busctl`-shaped, talks to it through [`GazeProxy`].
//!
//! Properties rather than a status struct so `busctl introspect` reads well, and so a
//! future signal per property costs nothing to add. A client wanting all of them at once
//! uses `org.freedesktop.DBus.Properties.GetAll` and [`Status::from_properties`], one
//! round trip.

use std::collections::HashMap;

use zbus::proxy;
use zbus::zvariant::OwnedValue;

/// The well-known bus name the daemon claims, and the interface name.
pub const BUS_NAME: &str = "dev.techgeek1.CosmicGaze";

/// The object path the interface is served at.
pub const BUS_PATH: &str = "/dev/techgeek1/CosmicGaze";

/// What the eyes are doing, as the `Mode` property spells it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// No tracker on the bus, or the session is not running.
    #[default]
    NoTracker,
    /// Paused from the applet: nothing is drawn or injected.
    Paused,
    /// Thumb up: the eyes read, and may scroll.
    Reading,
    /// Thumb on the pad or the latch on: controls are marked, a press clicks.
    Pointing,
    /// An edge scroll is running.
    Scrolling,
}

/// The daemon's properties, as one struct. Filled by the session loop, read by the
/// interface; decoded from a `GetAll` by the applet.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    /// The tracker is connected and streaming.
    pub tracker          : bool,
    /// A calibration file was loaded.
    pub calibrated       : bool,
    /// A residual model is running, and with it the online offset.
    pub model            : bool,
    /// The Daydream controller is connected.
    pub controller       : bool,
    /// Paused from the applet.
    pub paused           : bool,
    pub mode             : Mode,
    /// Accepted clicks folded into the offset over the life of its state.
    pub offset_updates   : u64,
    /// Bias jumps adopted from a consensus of rejects.
    pub offset_jumps     : u64,
    /// The offset's global mean, degrees.
    pub offset_yaw_deg   : f64,
    pub offset_pitch_deg : f64,
}

/// The interface, as the client sees it. `zbus` derives an async `GazeProxy` and a
/// `GazeProxyBlocking` from this.
#[proxy(
    interface       = "dev.techgeek1.CosmicGaze",
    default_service = "dev.techgeek1.CosmicGaze",
    default_path    = "/dev/techgeek1/CosmicGaze",
)]
pub trait Gaze {
    /// Stops drawing and injecting until `Resume`. The tracker keeps streaming.
    fn pause(&self) -> zbus::Result<()>;

    /// Undoes `Pause`.
    fn resume(&self) -> zbus::Result<()>;

    /// Forgets the day's offset: every anchor, back to the model alone.
    fn reset_offset(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn tracker(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn calibrated(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn model(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn controller(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn paused(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn mode(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn offset_updates(&self) -> zbus::Result<u64>;

    #[zbus(property)]
    fn offset_jumps(&self) -> zbus::Result<u64>;

    #[zbus(property)]
    fn offset_yaw_deg(&self) -> zbus::Result<f64>;

    #[zbus(property)]
    fn offset_pitch_deg(&self) -> zbus::Result<f64>;
}

// --- Mode ---

impl Mode {
    /// The property's string.
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::NoTracker => "no-tracker",
            Mode::Paused    => "paused",
            Mode::Reading   => "reading",
            Mode::Pointing  => "pointing",
            Mode::Scrolling => "scrolling",
        }
    }

    /// The mode a property string names, `NoTracker` for anything unknown: a newer
    /// daemon's mode is still a mode, and the applet has nothing better to show.
    pub fn parse(s: &str) -> Mode {
        match s {
            "paused"    => Mode::Paused,
            "reading"   => Mode::Reading,
            "pointing"  => Mode::Pointing,
            "scrolling" => Mode::Scrolling,
            _           => Mode::NoTracker,
        }
    }

    /// What to show a person.
    pub fn label(self) -> &'static str {
        match self {
            Mode::NoTracker => "No tracker",
            Mode::Paused    => "Paused",
            Mode::Reading   => "Reading",
            Mode::Pointing  => "Pointing",
            Mode::Scrolling => "Scrolling",
        }
    }
}

// --- Status ---

impl Status {
    /// Decodes a `Properties.GetAll` answer. A missing or mistyped property keeps its
    /// default, so an older or newer daemon still yields something to show.
    pub fn from_properties(props: &HashMap<String, OwnedValue>) -> Status {
        // Cloned per property: ten small values, once a second at most.
        fn get<T: TryFrom<OwnedValue>>(props: &HashMap<String, OwnedValue>, key: &str) -> Option<T> {
            props.get(key).and_then(|v| T::try_from(v.clone()).ok())
        }

        Status {
            tracker          : get(props, "Tracker").unwrap_or(false),
            calibrated       : get(props, "Calibrated").unwrap_or(false),
            model            : get(props, "Model").unwrap_or(false),
            controller       : get(props, "Controller").unwrap_or(false),
            paused           : get(props, "Paused").unwrap_or(false),
            mode             : get::<String>(props, "Mode").map(|s| Mode::parse(&s)).unwrap_or_default(),
            offset_updates   : get(props, "OffsetUpdates").unwrap_or(0),
            offset_jumps     : get(props, "OffsetJumps").unwrap_or(0),
            offset_yaw_deg   : get(props, "OffsetYawDeg").unwrap_or(0.0),
            offset_pitch_deg : get(props, "OffsetPitchDeg").unwrap_or(0.0),
        }
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::Value;

    #[test]
    fn modes_round_trip_through_their_strings() {
        for mode in [Mode::NoTracker, Mode::Paused, Mode::Reading, Mode::Pointing, Mode::Scrolling] {
            assert_eq!(Mode::parse(mode.as_str()), mode);
        }

        assert_eq!(Mode::parse("something-newer"), Mode::NoTracker);
    }

    /// The applet decodes whatever `GetAll` returned: present keys land, absent ones
    /// default, a wrong type defaults rather than failing the whole status.
    #[test]
    fn a_partial_property_map_still_decodes() {
        let mut props = HashMap::new();

        props.insert("Tracker".to_string(),       OwnedValue::try_from(Value::Bool(true)).unwrap());
        props.insert("Mode".to_string(),          OwnedValue::try_from(Value::from("scrolling")).unwrap());
        props.insert("OffsetUpdates".to_string(), OwnedValue::try_from(Value::U64(42)).unwrap());
        props.insert("OffsetYawDeg".to_string(),  OwnedValue::try_from(Value::from("not a number")).unwrap());

        let status = Status::from_properties(&props);

        assert!(status.tracker);
        assert!(!status.calibrated);
        assert_eq!(status.mode, Mode::Scrolling);
        assert_eq!(status.offset_updates, 42);
        assert_eq!(status.offset_yaw_deg, 0.0);
    }
}
