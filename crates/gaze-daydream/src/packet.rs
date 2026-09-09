//! The controller's 20-byte report and its bit layout.
//!
//! The Daydream controller notifies one packet per sensor tick, about 60 per second on the
//! desk, on the characteristic `00000001-1000-1000-8000-00805f9b34fb` of service `0xfe55`.
//! The layout is not published; it was reverse engineered by mrdoob for
//! `daydream-controller.js` and is reproduced here field for field. Every reading is a
//! 13-bit two's complement integer packed without byte alignment, followed by two 8-bit
//! touch coordinates and five button bits.
//!
//! ```text
//! byte  0        1        2        3        4        5        6        7        8        9
//!      tttttttt tsssssoo oooooooo oooooooo oooooooo oooooooo oooooaaa aaaaaaaa aaaaaaaa aaaaaaaa
//!      time(9)   seq(5) ori.x(13)   ori.y(13)     ori.z(13)     acc.x(13)  acc.y(13)
//! byte 10       11       12       13       14       15       16       17       18       19
//!      aaaaaaaa aaaagggg gggggggg gggggggg gggggggg gggggggg gggxxxxx xxxyyyyy yyybbbbb ????????
//!      acc.z(13)    gyro.x(13)  gyro.y(13)   gyro.z(13)  touch.x(8) touch.y(8) buttons
//! ```
//!
//! Byte 19 read `0x11` in every packet seen so far and is not decoded.

use std::f32::consts::PI;

use glam::{Vec2, Vec3};

/// Length of every report. Anything else is not a report.
pub const PACKET_LEN: usize = 20;

/// The five buttons, as bits of byte 18.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Button {
    /// The touchpad pressed down.
    Click,
    /// The circle-and-dash button under the touchpad. Long-pressing it wakes and pairs
    /// the controller, which the firmware handles before this bit is seen.
    Home,
    /// The minus-shaped button between the touchpad and Home.
    App,
    /// Volume down, on the side.
    VolumeDown,
    /// Volume up, on the side.
    VolumeUp,
}

impl Button {
    /// Every button, in bit order.
    pub const ALL: [Button; 5] = [
        Button::Click,
        Button::Home,
        Button::App,
        Button::VolumeDown,
        Button::VolumeUp,
    ];

    /// The bit this button occupies in byte 18.
    pub fn mask(self) -> u8 {
        match self {
            Button::Click      => 0x01,
            Button::Home       => 0x02,
            Button::App        => 0x04,
            Button::VolumeDown => 0x08,
            Button::VolumeUp   => 0x10,
        }
    }
}

/// The state of all five buttons in one report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Buttons(u8);

impl Buttons {
    /// The state with exactly these bits down, masked to the five that exist. For
    /// synthetic reports; a decoded packet already carries its own.
    pub fn from_bits(bits: u8) -> Buttons {
        Buttons(bits & 0x1F)
    }

    /// Whether `button` is down in this report.
    pub fn is_down(self, button: Button) -> bool {
        self.0 & button.mask() != 0
    }

    /// The buttons that are down now and were not in `before`: the press edges.
    pub fn pressed_since(self, before: Buttons) -> impl Iterator<Item = Button> {
        let edges = self.0 & !before.0;

        Button::ALL.into_iter().filter(move |b| edges & b.mask() != 0)
    }

    /// The buttons that were down in `before` and are not now: the release edges.
    pub fn released_since(self, before: Buttons) -> impl Iterator<Item = Button> {
        before.pressed_since(self)
    }

    /// The raw five bits.
    pub fn bits(self) -> u8 {
        self.0
    }
}

/// One decoded report.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Packet {
    /// The controller's 9-bit tick counter. Its unit is not known; it advances by about
    /// 16 per report at the observed 62 Hz, so it wraps every half second or so.
    pub time        : u16,
    /// A 5-bit report counter, one per notification, for spotting dropped packets.
    pub seq         : u8,
    /// The fused orientation as a rotation vector in radians, from the firmware's own
    /// sensor fusion. Drifts about the gravity axis over minutes.
    pub orientation : Vec3,
    /// Linear acceleration in metres per second squared, gravity included.
    pub accel       : Vec3,
    /// Angular velocity in radians per second.
    pub gyro        : Vec3,
    /// Where the thumb is on the touchpad, each axis in `0..=1`, or `None` while nothing
    /// touches it. `(0, 0)` on the wire means no touch; the pad's real corner reads a
    /// little above zero.
    pub touch       : Option<Vec2>,
    /// The buttons.
    pub buttons     : Buttons,
}

/// Full scale of a 13-bit reading.
const FULL_SCALE: f32 = 4095.0;

/// Orientation full scale is a whole turn.
const ORIENTATION_SCALE: f32 = 2.0 * PI / FULL_SCALE;

/// Acceleration full scale is 8 g.
const ACCEL_SCALE: f32 = 8.0 * 9.8 / FULL_SCALE;

/// Gyro full scale is 2048 degrees per second.
const GYRO_SCALE: f32 = 2048.0 / 180.0 * PI / FULL_SCALE;

// --- Decoding ---

/// Decodes one notification. `None` when it is not [`PACKET_LEN`] bytes long.
pub fn decode(d: &[u8]) -> Option<Packet> {
    if d.len() != PACKET_LEN {
        return None;
    }

    let d: [u32; PACKET_LEN] = std::array::from_fn(|i| u32::from(d[i]));

    let time = ((d[0] & 0xFF) << 1 | (d[1] & 0x80) >> 7) as u16;
    let seq  = ((d[1] & 0x7C) >> 2) as u8;

    let orientation = Vec3::new(
        signed13((d[1] & 0x03) << 11 | (d[2] & 0xFF) << 3 | (d[3] & 0x80) >> 5),
        signed13((d[3] & 0x1F) << 8  | (d[4] & 0xFF)),
        signed13((d[5] & 0xFF) << 5  | (d[6] & 0xF8) >> 3),
    ) * ORIENTATION_SCALE;

    let accel = Vec3::new(
        signed13((d[6] & 0x07) << 10 | (d[7] & 0xFF) << 2 | (d[8] & 0xC0) >> 6),
        signed13((d[8] & 0x3F) << 7  | (d[9] & 0xFE) >> 1),
        signed13((d[9] & 0x01) << 12 | (d[10] & 0xFF) << 4 | (d[11] & 0xF0) >> 4),
    ) * ACCEL_SCALE;

    let gyro = Vec3::new(
        signed13((d[11] & 0x0F) << 9 | (d[12] & 0xFF) << 1 | (d[13] & 0x80) >> 7),
        signed13((d[13] & 0x7F) << 6 | (d[14] & 0xFC) >> 2),
        signed13((d[14] & 0x03) << 11 | (d[15] & 0xFF) << 3 | (d[16] & 0xE0) >> 5),
    ) * GYRO_SCALE;

    let touch_x = (d[16] & 0x1F) << 3 | (d[17] & 0xE0) >> 5;
    let touch_y = (d[17] & 0x1F) << 3 | (d[18] & 0xE0) >> 5;

    let touch = (touch_x != 0 || touch_y != 0)
        .then(|| Vec2::new(touch_x as f32 / 255.0, touch_y as f32 / 255.0));

    Some(Packet {
        time        : time,
        seq         : seq,
        orientation : orientation,
        accel       : accel,
        gyro        : gyro,
        touch       : touch,
        buttons     : Buttons((d[18] & 0x1F) as u8),
    })
}

/// Sign-extends a 13-bit two's complement reading.
fn signed13(raw: u32) -> f32 {
    let raw = raw & 0x1FFF;

    match raw & 0x1000 {
        0 => raw as f32,
        _ => raw as f32 - 8192.0,
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    /// A report captured 2026-09-04 with the controller lying still on the desk, decoded
    /// by hand from the layout in the module docs: orientation `(-20, 1884, 9)` raw, gyro
    /// `(0, 0, 0)`, no touch, no buttons, time 191, seq 13. The same layout run as the
    /// reference JavaScript against the live stream gave the same at-rest orientation
    /// within two counts, which is the sensor's own jitter.
    #[test]
    fn a_captured_at_rest_report_decodes_to_the_reference_figures() {
        let p = decode(&hex("5fb7fdc75c004ffcc441ffe00000000000000011")).unwrap();

        assert_eq!(p.time, 191);
        assert_eq!(p.seq, 13);

        let ori = p.orientation / ORIENTATION_SCALE;

        assert_eq!(ori.round(), Vec3::new(-20.0, 1884.0, 9.0));
        assert_eq!(p.gyro, Vec3::ZERO);
        assert_eq!(p.touch, None);
        assert_eq!(p.buttons, Buttons::default());

        // Lying flat, the accelerometer sees about one g in total.
        let g = p.accel.length();

        assert!((8.5..11.0).contains(&g), "gravity read {g}");
    }

    /// A report from the same capture whose gyro z field was the only thing that differed:
    /// all thirteen bits set, which is minus one and is what tells the sign extension
    /// from a shift.
    #[test]
    fn a_small_negative_gyro_reading_sign_extends() {
        let p = decode(&hex("7f47fdc75c004ffcc441ffe0000003ffe0000011")).unwrap();

        let gyro = p.gyro / GYRO_SCALE;

        assert_eq!(gyro.round(), Vec3::new(0.0, 0.0, -1.0));
    }

    #[test]
    fn sign_extension_covers_both_ends_of_the_range() {
        assert_eq!(signed13(0), 0.0);
        assert_eq!(signed13(4095), 4095.0);
        assert_eq!(signed13(0x1FFF), -1.0);
        assert_eq!(signed13(0x1000), -4096.0);
    }

    #[test]
    fn the_wrong_length_is_not_a_packet() {
        assert_eq!(decode(&[0; 19]), None);
        assert_eq!(decode(&[0; 21]), None);
    }

    /// The button bits sit in the low five of byte 18 and touch y's low three bits sit
    /// in its high three, so a click with the thumb at the pad's far edge must keep both.
    #[test]
    fn buttons_and_touch_share_byte_eighteen_without_bleeding() {
        let mut d = hex("5fb7fdc75c004ffcc441ffe00000000000000011");

        d[16] |= 0x1F;
        d[17]  = 0xFF;
        d[18]  = 0xE0 | Button::Click.mask() | Button::VolumeUp.mask();

        let p = decode(&d).unwrap();

        assert_eq!(p.touch, Some(Vec2::new(1.0, 1.0)));
        assert!(p.buttons.is_down(Button::Click));
        assert!(p.buttons.is_down(Button::VolumeUp));
        assert!(!p.buttons.is_down(Button::App));
    }

    #[test]
    fn press_and_release_edges_come_from_consecutive_states() {
        let before = Buttons(Button::Home.mask() | Button::Click.mask());
        let after  = Buttons(Button::Home.mask() | Button::App.mask());

        assert_eq!(after.pressed_since(before).collect::<Vec<_>>(),  vec![Button::App]);
        assert_eq!(after.released_since(before).collect::<Vec<_>>(), vec![Button::Click]);
        assert_eq!(after.pressed_since(after).count(), 0);
    }
}
