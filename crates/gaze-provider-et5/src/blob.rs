//! Identity and comparison of the on-device calibration blob.
//!
//! The blob is opaque: a few hundred kilobytes of firmware eye model with no known
//! internal structure. The only things that can honestly be said about one are its
//! length, its hash, and where two of them first diverge, so those are what the
//! diagnostics print, what the calibration file records, and what the connect-time
//! verification decides on.
//!
//! Whether `cal_retrieve` is deterministic (the same bytes back on two reads with no
//! intervening upload) is a device question `gaze-et5-cli blob-info` answers; the
//! answer picks the [`BlobCheck`] the provider should use.

use sha2::{Digest, Sha256};

// --- BlobReport ---

/// Size and hash of one blob: the identity used in logs, in `blob-info` output, and
/// in the calibration file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobReport {
    /// Length in bytes.
    pub len    : usize,
    /// Lowercase hex SHA-256 of the whole blob.
    pub sha256 : String,
}

impl BlobReport {
    /// Reports on a blob's bytes.
    pub fn of(bytes: &[u8]) -> Self {
        Self {
            len    : bytes.len(),
            sha256 : sha256_hex(bytes),
        }
    }

    /// The first eight hex characters of the hash, for log lines and history keys.
    /// Long enough to name a blob among the handful a desk ever sees.
    pub fn short(&self) -> &str {
        &self.sha256[..8]
    }
}

impl std::fmt::Display for BlobReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} bytes, sha256 {}", self.len, self.sha256)
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
    /// table, see the module docs), and uploading the read-back form hands the original
    /// trailer back again. Kept for tests and for firmware that behaves.
    Exact,
    /// Same length, and the first `prefix_len` bytes identical. A shorter blob is
    /// compared over all of it.
    SizeAndPrefix { prefix_len: usize },
    /// Same length, and everything but the last `trailer_max` bytes identical: the
    /// body of the blob is the model, the trailer is device-managed. The default.
    Body { trailer_max: usize },
}

/// Upper bound on the device-managed trailer, bytes. Measured at 520 for 13 unique
/// calibration targets (40 bytes per target); 2048 leaves room for a 50-target model.
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
                if expected.len() != actual.len() {
                    return false;
                }

                let n = expected.len().saturating_sub(*trailer_max);

                expected[..n] == actual[..n]
            }
        }
    }
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

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(report.short(), "ba7816bf");
        assert!(report.to_string().starts_with("3 bytes, sha256 ba7816bf"));
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

        assert!(check.agrees(b"abcdef", b"abcdXY"));
        assert!(!check.agrees(b"abcdef", b"abcXef"));
        assert!(!check.agrees(b"abcdef", b"abcdefg"));
        // A blob shorter than the trailer bound is all trailer: only the length counts.
        assert!(check.agrees(b"a", b"b"));
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
}
