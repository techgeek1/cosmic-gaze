//! The line protocol spoken by the Python gaze sidecar over its Unix socket.
//!
//! One JSON object per line, roughly 30 Hz:
//!
//! ```text
//! {"t": 12.5, "seq": 375, "valid": true, "eye_mm": [x, y, z], "gaze": [dx, dy, dz],
//!  "head_rot": [rx, ry, rz], "conf": 0.83, "lat_ms": 41.2}
//! ```
//!
//! `eye_mm` and `gaze` are in the OpenCV camera frame (see `crate::camera`): millimetres
//! and a unit vector respectively. The vector fields are null on an invalid frame.
//!
//! # What this parser assumes
//!
//! The sidecar is being written concurrently, so the reader is deliberately forgiving:
//! every field has a default, unknown fields are ignored, and a line that fails to parse
//! is logged and skipped rather than treated as a disconnect. A message whose `valid` is
//! true but whose `eye_mm` or `gaze` is missing is downgraded to invalid, because there is
//! nothing to build a ray from. `head_rot` is carried through uninterpreted: no rotation
//! convention for it has been agreed, and nothing in this crate needs one yet.

use glam::DVec3;
use serde::{Deserialize, Serialize};

/// One line of the sidecar stream, as it arrives.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SidecarMessage {
    /// Monotonic seconds on the sidecar's clock. Not comparable with this process's
    /// clock; the provider re-bases it (see `crate::provider`).
    #[serde(default)]
    pub t        : f64,
    /// Sidecar frame counter. Gaps mean dropped frames.
    #[serde(default)]
    pub seq      : u64,
    /// False when the sidecar could not find or track a face this frame.
    #[serde(default)]
    pub valid    : bool,
    /// Eye midpoint in camera-frame millimetres. Null on an invalid frame.
    #[serde(default)]
    pub eye_mm   : Option<[f64; 3]>,
    /// Gaze direction as a camera-frame unit vector. Null on an invalid frame.
    #[serde(default)]
    pub gaze     : Option<[f64; 3]>,
    /// Head rotation as reported by the sidecar, convention unspecified and unused here.
    #[serde(default)]
    pub head_rot : Option<[f64; 3]>,
    /// The model's own confidence in this frame, nominally [0, 1].
    #[serde(default)]
    pub conf     : f64,
    /// Sidecar-side latency, capture to socket write, milliseconds.
    #[serde(default)]
    pub lat_ms   : f64,
}

/// A message that carried a usable eye and gaze vector, already in `glam` form.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SidecarGaze {
    pub t        : f64,
    pub seq      : u64,
    /// Eye midpoint, camera frame, millimetres.
    pub eye_mm   : DVec3,
    /// Gaze direction, camera frame, unit length.
    pub gaze     : DVec3,
    pub conf     : f64,
    pub lat_ms   : f64,
}

// --- SidecarMessage ---

impl SidecarMessage {
    /// Parses one line. Leading and trailing whitespace is tolerated; a blank line is
    /// `Ok(None)` rather than an error so a sidecar that flushes a stray newline does not
    /// look like a protocol failure.
    pub fn parse(line: &str) -> Result<Option<Self>, ProtocolError> {
        let trimmed = line.trim();

        if trimmed.is_empty() {
            return Ok(None);
        }

        serde_json::from_str(trimmed)
            .map(Some)
            .map_err(|source| ProtocolError::Json { line: trimmed.to_string(), source })
    }

    /// Serialises back to a single line, no trailing newline. Used by the fake sidecar and
    /// by tests.
    pub fn to_line(&self) -> Result<String, ProtocolError> {
        serde_json::to_string(self).map_err(ProtocolError::Encode)
    }

    /// The usable part of the message, or `None` when the sidecar reported invalid or left
    /// out a vector it needs to be valid.
    pub fn gaze(&self) -> Option<SidecarGaze> {
        if !self.valid {
            return None;
        }

        let eye  = DVec3::from_array(self.eye_mm?);
        let gaze = DVec3::from_array(self.gaze?);

        // A NaN slipping through from the model would silently poison the geometry, and a
        // zero-length gaze vector has no direction to speak of.
        if !eye.is_finite() || !gaze.is_finite() || gaze.length_squared() <= 0.0 {
            return None;
        }

        Some(SidecarGaze {
            t      : self.t,
            seq    : self.seq,
            eye_mm : eye,
            gaze   : gaze.normalize(),
            conf   : self.conf,
            lat_ms : self.lat_ms,
        })
    }

    /// Builds a valid message from desk-side quantities already converted to the camera
    /// frame. The fake sidecar's only way of producing a line.
    pub fn from_gaze(g: &SidecarGaze) -> Self {
        Self {
            t        : g.t,
            seq      : g.seq,
            valid    : true,
            eye_mm   : Some(g.eye_mm.to_array()),
            gaze     : Some(g.gaze.to_array()),
            head_rot : Some([0.0, 0.0, 0.0]),
            conf     : g.conf,
            lat_ms   : g.lat_ms,
        }
    }

    /// The message a sidecar sends when it has lost the face.
    pub fn invalid(t: f64, seq: u64) -> Self {
        Self { t: t, seq: seq, valid: false, ..Self::default() }
    }
}

// --- Error ---

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("malformed sidecar line {line:?}: {source}")]
    Json { line: String, #[source] source: serde_json::Error },

    #[error("cannot encode sidecar message: {0}")]
    Encode(#[source] serde_json::Error),
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"{"t":12.5,"seq":375,"valid":true,"eye_mm":[10.0,-20.0,600.0],"gaze":[0.0,0.1,0.99],"head_rot":[0.0,0.0,0.0],"conf":0.83,"lat_ms":41.2}"#;

    const INVALID: &str = r#"{"t":12.6,"seq":376,"valid":false,"eye_mm":null,"gaze":null,"head_rot":null,"conf":0.0,"lat_ms":40.0}"#;

    #[test]
    fn parses_a_valid_line() {
        let m = SidecarMessage::parse(VALID).unwrap().unwrap();

        assert_eq!(m.seq, 375);
        assert!(m.valid);

        let g = m.gaze().expect("valid line must yield a gaze");
        assert_eq!(g.eye_mm, DVec3::new(10.0, -20.0, 600.0));
        assert!((g.gaze.length() - 1.0).abs() < 1.0e-12, "gaze must come out unit length");
        assert_eq!(g.conf, 0.83);
        assert_eq!(g.lat_ms, 41.2);
    }

    #[test]
    fn nulls_on_an_invalid_line_are_tolerated() {
        let m = SidecarMessage::parse(INVALID).unwrap().unwrap();

        assert!(!m.valid);
        assert!(m.gaze().is_none());
    }

    #[test]
    fn a_valid_flag_without_vectors_is_downgraded() {
        let m = SidecarMessage::parse(r#"{"t":1.0,"seq":1,"valid":true}"#).unwrap().unwrap();

        assert!(m.valid);
        assert!(m.gaze().is_none(), "no vectors means nothing to build a ray from");
    }

    #[test]
    fn missing_and_unknown_fields_are_both_survivable() {
        // Everything defaults, and a field the sidecar added later is ignored.
        let m = SidecarMessage::parse(r#"{"seq":9,"blink":true}"#).unwrap().unwrap();

        assert_eq!(m.seq, 9);
        assert!(!m.valid);
    }

    #[test]
    fn a_blank_line_is_not_an_error() {
        assert_eq!(SidecarMessage::parse("   ").unwrap(), None);
        assert_eq!(SidecarMessage::parse("").unwrap(), None);
    }

    #[test]
    fn a_malformed_line_reports_the_text_it_choked_on() {
        let err = SidecarMessage::parse("not json").unwrap_err();

        assert!(matches!(err, ProtocolError::Json { .. }));
        assert!(err.to_string().contains("not json"));
    }

    #[test]
    fn nan_and_zero_vectors_are_rejected() {
        let nan = SidecarMessage {
            valid  : true,
            eye_mm : Some([0.0, 0.0, f64::NAN]),
            gaze   : Some([0.0, 0.0, 1.0]),
            ..SidecarMessage::default()
        };
        assert!(nan.gaze().is_none());

        let zero = SidecarMessage {
            valid  : true,
            eye_mm : Some([0.0, 0.0, 600.0]),
            gaze   : Some([0.0, 0.0, 0.0]),
            ..SidecarMessage::default()
        };
        assert!(zero.gaze().is_none());
    }

    #[test]
    fn round_trips_through_a_line() {
        let g = SidecarGaze {
            t      : 3.25,
            seq    : 97,
            eye_mm : DVec3::new(1.0, 2.0, 3.0),
            gaze   : DVec3::new(0.0, 0.0, 1.0),
            conf   : 0.5,
            lat_ms : 33.0,
        };

        let line = SidecarMessage::from_gaze(&g).to_line().unwrap();
        let back = SidecarMessage::parse(&line).unwrap().unwrap().gaze().unwrap();

        assert_eq!(back, g);
    }
}
