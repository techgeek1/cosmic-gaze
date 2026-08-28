//! Identity and comparison of the on-device calibration blob.
//!
//! The blob is a few hundred kilobytes of firmware eye model with no known internal
//! structure, followed by one thing that *is* readable: the firmware's own per-point
//! calibration result table. [`decode_trailer`] finds and decodes it, [`body`] is
//! everything before it, and the body is the blob's identity. That split is what makes
//! a blob comparable at all: the body round-trips an upload byte for byte, the trailer
//! does not.
//!
//! The trailer is not device scratch. Measured 2026-08-28 on a 604948 byte blob, the
//! last 520 bytes are 13 records of 40 bytes, one per unique target of the retrain
//! ceremony (`sweep.rs`'s ring plus the lean dots), each holding the target and the
//! position each eye was measured at. Reading a blob back re-expresses that table in
//! whatever display area is declared *at read time*: the same table retrieved under
//! the trained plane and under the oversized virtual plane differs by exactly the
//! affine map between the two planes (measured: scale 0.3669 across and 0.2604 down,
//! matching the 874.6 x 364.6 mm trained area inside the 2400 x 1400 mm virtual one,
//! to 0.6%). The trailer bytes are therefore a *view* of device state, not state, and
//! comparing them across a round trip means nothing. Bodies are compared; trailers are
//! decoded and reported.
//!
//! One thing here is a guess: which of the two per-eye slots is the left eye. The
//! record is target, then eye A, then eye B, and left-then-right is the order Tobii's
//! own per-point result tables use. Nothing in this crate depends on it being right;
//! getting it wrong only swaps two columns of `blob-info`.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Bytes per record of the trailer's calibration result table.
pub const RESULT_RECORD_BYTES: usize = 40;

/// Range a target coordinate must fall in to be read as a real record, normalised
/// display units. A live target sits in [0, 1]; the slack admits one pushed off the
/// declared plane by the read-time re-normalisation while still rejecting the
/// arbitrary floats of the model body.
const TARGET_RANGE: std::ops::RangeInclusive<f32> = -0.5..=1.5;

// --- CalibrationResult ---

/// Where one eye was measured looking while a calibration target was shown.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct EyeResult {
    /// Measured gaze position across and down the declared display area, normalised.
    pub position : [f32; 2],
    /// The firmware's own validity flag for this eye at this target.
    pub valid    : bool,
}

/// One row of the firmware's per-point calibration result table: a target and what
/// each eye actually did while it was shown.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PointResult {
    /// Target position across and down the declared display area, normalised.
    pub target : [f32; 2],
    /// The left eye, assuming the eye-order guess in the module docs holds.
    pub left   : EyeResult,
    /// The right eye, same assumption.
    pub right  : EyeResult,
}

impl PointResult {
    /// Distance between the target and an eye's measured position, normalised units.
    /// The per-eye error the firmware itself accepted, which is as close to a free
    /// quality number as this device gives.
    pub fn error(&self, eye: &EyeResult) -> f32 {
        let du = eye.position[0] - self.target[0];
        let dv = eye.position[1] - self.target[1];

        (du * du + dv * dv).sqrt()
    }
}

/// The firmware's report on the calibration it last committed: one row per unique
/// target, in the order the device stores them.
///
/// Read out of a blob's trailer, so the coordinates are normalised against whichever
/// display area was declared when the blob was retrieved (see the module docs). Two
/// tables are only comparable number for number when both were read under the same
/// plane.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CalibrationResult {
    /// One row per target.
    pub targets : Vec<PointResult>,
}

// --- BlobReport ---

/// What can honestly be said about one blob: its length, the identity of its model
/// body, and the shape of its trailer. Used in logs, in `blob-info` output, and in the
/// calibration file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobReport {
    /// Length of the whole blob, bytes.
    pub len             : usize,
    /// Lowercase hex SHA-256 of the whole blob. Diagnostic only: it changes across a
    /// round trip that changed nothing.
    pub sha256          : String,
    /// Length of the model body, bytes: everything before the decoded trailer, or the
    /// whole blob when no trailer decodes.
    pub body_len        : usize,
    /// Lowercase hex SHA-256 of the body. **This is the blob's identity**, the thing
    /// session files and calibration files are keyed to.
    pub body_sha256     : String,
    /// Rows of the decoded result table, `None` when no trailer decodes.
    pub trailer_records : Option<usize>,
}

impl BlobReport {
    /// Reports on a blob's bytes.
    pub fn of(bytes: &[u8]) -> Self {
        let trailer = decode_trailer(bytes);
        let body    = &bytes[..bytes.len() - trailer.as_ref().map_or(0, |(n, _)| *n)];

        Self {
            len             : bytes.len(),
            sha256          : sha256_hex(bytes),
            body_len        : body.len(),
            body_sha256     : sha256_hex(body),
            trailer_records : trailer.map(|(_, table)| table.targets.len()),
        }
    }

    /// The first eight hex characters of the body hash, for log lines and history
    /// keys. Long enough to name a blob among the handful a desk ever sees.
    pub fn short(&self) -> &str {
        &self.body_sha256[..8]
    }

    /// Length of the trailer, bytes.
    pub fn trailer_len(&self) -> usize {
        self.len - self.body_len
    }
}

impl std::fmt::Display for BlobReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} bytes, body {} sha256 {}", self.len, self.body_len,
               self.body_sha256)?;

        match self.trailer_records {
            Some(rows) => write!(f, ", {} byte trailer of {rows} points",
                                 self.trailer_len()),
            None       => write!(f, ", no result trailer"),
        }
    }
}

// --- BlobCheck ---

/// How a retrieved blob is compared against the one that was uploaded.
///
/// `Exact` is only correct if the firmware hands back exactly what it was given;
/// `SizeAndPrefix` is the fallback for a firmware that re-serialises the model (float
/// noise, a timestamp, a scratch region) and so returns different bytes for the same
/// model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlobCheck {
    /// Byte-identical, the strongest statement available. Not what the firmware does:
    /// measured 2026-08-28, an upload round-trips its first 604428 of 604948 bytes
    /// exactly and the last 520 come back different (the per-point calibration result
    /// table, see the module docs). Kept for tests and for firmware that behaves.
    Exact,
    /// Same length, and the first `prefix_len` bytes identical. A shorter blob is
    /// compared over all of it.
    SizeAndPrefix { prefix_len: usize },
    /// The model bodies identical, and the same number of points in both result
    /// tables. When either blob has no decodable trailer this falls back to "same
    /// length, and everything but the last `trailer_max` bytes identical". The
    /// default.
    Body { trailer_max: usize },
}

/// Upper bound on the device-managed trailer, bytes, for the fallback rule. Measured
/// at 520 for 13 unique calibration targets (40 bytes per target); 2048 leaves room
/// for a 50-target model.
pub const TRAILER_MAX_BYTES: usize = 2048;

impl Default for BlobCheck {
    fn default() -> Self {
        Self::Body { trailer_max: TRAILER_MAX_BYTES }
    }
}

impl BlobCheck {
    /// Whether the retrieved `actual` satisfies this check against the uploaded
    /// `expected`.
    pub fn agrees(&self, expected: &[u8], actual: &[u8]) -> bool {
        match self {
            Self::Exact                        => expected == actual,
            Self::SizeAndPrefix { prefix_len } => {
                if expected.len() != actual.len() {
                    return false;
                }

                let n = (*prefix_len).min(expected.len());

                expected[..n] == actual[..n]
            }
            Self::Body { trailer_max }         => {
                // With both trailers decoded the split is known exactly, so compare
                // the bodies and nothing else. Equal record counts is the one thing
                // worth demanding of the trailers: the numbers in them are normalised
                // against the plane declared at read time and legitimately differ.
                if let (Some((_, want)), Some((_, got)))
                    = (decode_trailer(expected), decode_trailer(actual))
                {
                    return want.targets.len() == got.targets.len()
                        && body(expected) == body(actual);
                }

                if expected.len() != actual.len() {
                    return false;
                }

                let n = expected.len().saturating_sub(*trailer_max);

                expected[..n] == actual[..n]
            }
        }
    }
}

// --- Trailer ---

/// Decodes the firmware's per-point calibration result table off the end of a blob,
/// returning its length in bytes and the table itself.
///
/// Scans backwards in 40-byte strides for as long as the bytes look like a record
/// (both validity words 0 or 1, the target finite and on or near the declared plane)
/// and stops at the first stride that does not, which is where the opaque model body
/// begins. `None` when the last stride already fails, which is the honest answer for a
/// factory-default blob and for anything that is not one of this device's blobs.
///
/// The 128 bits of validity flags per record make a false positive off the body's
/// arbitrary floats effectively impossible, so the boundary this finds is the real
/// one rather than a guess bounded by [`TRAILER_MAX_BYTES`].
pub fn decode_trailer(blob: &[u8]) -> Option<(usize, CalibrationResult)> {
    let mut targets = Vec::new();
    let mut end     = blob.len();

    while end >= RESULT_RECORD_BYTES {
        let start = end - RESULT_RECORD_BYTES;
        let Some(record) = decode_record(&blob[start..end]) else {
            break;
        };

        targets.push(record);
        end = start;
    }

    if targets.is_empty() {
        return None;
    }

    // The scan ran from the end of the blob; the table reads forwards.
    targets.reverse();

    Some((blob.len() - end, CalibrationResult { targets: targets }))
}

/// The model body: everything before the decoded trailer, or the whole blob when no
/// trailer decodes. This is what an upload round-trips unchanged and what the blob's
/// identity is taken over.
pub fn body(blob: &[u8]) -> &[u8] {
    let trailer = decode_trailer(blob).map_or(0, |(len, _)| len);

    &blob[..blob.len() - trailer]
}

/// Lowercase hex SHA-256 of a blob's body: the identity of one on-device eye model.
pub fn body_sha256_hex(blob: &[u8]) -> String {
    sha256_hex(body(blob))
}

// --- Helpers ---

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);

    hex::encode(hasher.finalize())
}

/// Offset of the first byte at which two blobs differ, `None` when they are equal.
/// When one is a strict prefix of the other, that is its length: the first offset at
/// which they stop agreeing.
pub fn first_difference(a: &[u8], b: &[u8]) -> Option<usize> {
    let common = common_prefix_len(a, b);

    (a.len() != b.len() || common != a.len()).then_some(common)
}

/// Length of the longest shared leading run of two blobs.
pub fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// Decodes one 40-byte record, `None` when the bytes are not one.
///
/// Layout, all little-endian: target u and v as f32, then per eye a position as two
/// f32 followed by a 64-bit validity word. The validity words are what makes the
/// record recognisable; the coordinates only have to be plausible.
fn decode_record(bytes: &[u8]) -> Option<PointResult> {
    let f32_at = |offset: usize| {
        f32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("4 bytes"))
    };
    let flag_at = |offset: usize| {
        u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("8 bytes"))
    };

    let target = [f32_at(0), f32_at(4)];
    let left   = [f32_at(8), f32_at(12)];
    let right  = [f32_at(24), f32_at(28)];

    let (left_flag, right_flag) = (flag_at(16), flag_at(32));

    if left_flag > 1 || right_flag > 1 {
        return None;
    }

    if !target.iter().all(|c| c.is_finite() && TARGET_RANGE.contains(c)) {
        return None;
    }

    if !left.iter().chain(&right).all(|c| c.is_finite()) {
        return None;
    }

    Some(PointResult {
        target : target,
        left   : EyeResult { position: left , valid: left_flag  == 1 },
        right  : EyeResult { position: right, valid: right_flag == 1 },
    })
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// Last 1024 bytes of the blob committed by the 2026-08-27 16:12 retrain, whose
    /// trailer is normalised against the trained 874.6 x 364.6 mm plane.
    const COMMITTED: &[u8] = include_bytes!("../tests/fixtures/blob-tail-committed.bin");

    /// Last 1024 bytes of the same blob read back off the device with the oversized
    /// virtual plane declared, so the same table in different normalised units.
    const READBACK: &[u8] = include_bytes!("../tests/fixtures/blob-tail-readback.bin");

    #[test]
    fn sha256_matches_the_published_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        );
    }

    #[test]
    fn report_carries_length_and_a_short_form() {
        let report = BlobReport::of(b"abc");

        assert_eq!(report.len, 3);
        assert_eq!(report.body_len, 3);
        assert_eq!(report.trailer_records, None);
        // Nothing decodes, so the body is the blob and the identity is its hash.
        assert_eq!(report.sha256, report.body_sha256);
        assert_eq!(report.short(), "ba7816bf");
        assert!(report.to_string().starts_with("3 bytes, body 3 sha256 ba7816bf"));
    }

    #[test]
    fn first_difference_finds_the_divergence() {
        assert_eq!(first_difference(b"abcd", b"abcd"), None);
        assert_eq!(first_difference(b"abcd", b"abXd"), Some(2));

        // A strict prefix diverges where it runs out.
        assert_eq!(first_difference(b"abc", b"abcd"), Some(3));
        assert_eq!(first_difference(b"abcd", b"abc"), Some(3));
        assert_eq!(first_difference(b"", b"a"), Some(0));

        assert_eq!(common_prefix_len(b"abcd", b"abXd"), 2);
    }

    #[test]
    fn exact_check_demands_every_byte() {
        let exact = BlobCheck::Exact;

        assert!(exact.agrees(b"abcd", b"abcd"));
        assert!(!exact.agrees(b"abcd", b"abXd"));
        assert!(!exact.agrees(b"abcd", b"abc"));
    }

    #[test]
    fn body_check_ignores_only_the_trailer() {
        let check = BlobCheck::Body { trailer_max: 2 };

        // Nothing here decodes as a result table, so the byte-count fallback runs.
        assert!(check.agrees(b"abcdef", b"abcdXY"));
        assert!(!check.agrees(b"abcdef", b"abcXef"));
        assert!(!check.agrees(b"abcdef", b"abcdefg"));
        // A blob shorter than the trailer bound is all trailer: only the length counts.
        assert!(check.agrees(b"a", b"b"));
    }

    #[test]
    fn body_check_accepts_a_re_normalised_trailer() {
        let check = BlobCheck::default();

        // The real round trip: identical bodies, 13 points either side, and 520 bytes
        // of trailer that differ in every record.
        assert!(check.agrees(COMMITTED, READBACK));
        assert!(first_difference(COMMITTED, READBACK).is_some());

        // A changed body is still a mismatch, trailer or no trailer.
        let mut damaged = COMMITTED.to_vec();
        damaged[7] ^= 0xff;

        assert!(!check.agrees(COMMITTED, &damaged));
    }

    #[test]
    fn prefix_check_ignores_the_tail_but_not_the_length() {
        let check = BlobCheck::SizeAndPrefix { prefix_len: 3 };

        assert!(check.agrees(b"abcd", b"abcZ"));
        assert!(!check.agrees(b"abcd", b"abZd"));

        // A changed length is always a mismatch, whatever the prefix says.
        assert!(!check.agrees(b"abcd", b"abcde"));

        // A prefix longer than the blob compares all of it.
        let long = BlobCheck::SizeAndPrefix { prefix_len: 64 };
        assert!(long.agrees(b"ab", b"ab"));
        assert!(!long.agrees(b"ab", b"aX"));
    }

    #[test]
    fn trailer_decodes_the_retrains_thirteen_targets() {
        let (len, table) = decode_trailer(COMMITTED).expect("the fixture has a trailer");

        assert_eq!(len, 520);
        assert_eq!(table.targets.len(), 13);

        // The ring (centre, right, then anticlockwise around RING_RX/RING_RY) followed
        // by the four lean dots, exactly the order `sweep.rs` lays them out.
        let expected = [
            [0.50, 0.50], [0.70, 0.50], [0.50, 0.80], [0.30, 0.50],
            [0.35, 0.50], [0.65, 0.50], [0.50, 0.35], [0.50, 0.65],
        ];

        for want in expected {
            assert!(
                table.targets.iter().any(|p| {
                    (p.target[0] - want[0]).abs() < 1e-4
                        && (p.target[1] - want[1]).abs() < 1e-4
                }),
                "target {want:?} missing from {:?}",
                table.targets.iter().map(|p| p.target).collect::<Vec<_>>(),
            );
        }

        // The firmware only stores points it accepted, so every eye is valid.
        assert!(table.targets.iter().all(|p| p.left.valid && p.right.valid));

        // The ring points were gated to a few degrees; the lean dots were measured
        // with the head moved and are much further out. Both are finite and on-plane.
        assert!(table.targets[0].error(&table.targets[0].left) < 0.02);
    }

    #[test]
    fn the_two_fixtures_share_a_body_but_not_a_trailer() {
        let committed = BlobReport::of(COMMITTED);
        let readback  = BlobReport::of(READBACK);

        assert_eq!(committed.body_sha256, readback.body_sha256);
        assert_eq!(committed.trailer_records, Some(13));
        assert_eq!(readback.trailer_records, Some(13));
        assert_eq!(body_sha256_hex(COMMITTED), body_sha256_hex(READBACK));

        // The whole-blob hashes are the thing that misleads: same model, different
        // digest, which is why identity is the body's.
        assert_ne!(committed.sha256, readback.sha256);

        // Same table under a different plane: every target moved, none by much more
        // than the affine map between the two planes allows.
        let (_, want) = decode_trailer(COMMITTED).expect("trailer");
        let (_, got)  = decode_trailer(READBACK).expect("trailer");

        for (a, b) in want.targets.iter().zip(&got.targets) {
            assert_ne!(a.target, b.target);
            assert_eq!(a.left.valid, b.left.valid);
        }
    }

    #[test]
    fn a_blob_with_no_table_has_no_trailer() {
        // The factory default is ~1.5 KB of model and nothing else; a run of zeroes
        // stands in for "bytes that are not a result table". A zeroed record would
        // decode (flags 0, target 0,0), so the stand-in has to be non-zero.
        let noise = vec![0x5a; 4096];

        assert_eq!(decode_trailer(&noise), None);
        assert_eq!(body(&noise).len(), noise.len());
        assert_eq!(body_sha256_hex(&noise), sha256_hex(&noise));
    }
}
