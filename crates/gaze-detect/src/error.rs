//! The crate's error type. Everything fallible in `gaze-detect` is either onnxruntime
//! failing, a model file being missing or the wrong shape, or a poisoned session lock.

use std::path::PathBuf;

/// Convenience alias for results carrying a `DetectError`.
pub type Result<T> = core::result::Result<T, DetectError>;

// --- Error ---

/// Everything that can go wrong loading or running the detector.
#[derive(Debug, thiserror::Error)]
pub enum DetectError {
    /// The onnxruntime shared library could not be found or loaded. On this desktop the
    /// system library is `/usr/lib/libonnxruntime.so`; `ORT_DYLIB_PATH` overrides the
    /// search.
    #[error("could not load onnxruntime: {0}")]
    Runtime(String),

    /// A model file was not present under the models directory.
    #[error("model file not found: {path} (run crates/gaze-detect/scripts/fetch_models.py)")]
    MissingModel {
        /// Path that was looked for.
        path : PathBuf,
    },

    /// A model loaded but does not have the input or output shape this crate expects.
    #[error("model {path} is not usable: {what}")]
    BadModel {
        /// Path of the offending model.
        path : PathBuf,
        /// What was wrong with it.
        what : String,
    },

    /// The caller passed a buffer that is not `w * h * 4` bytes.
    #[error("frame buffer is {got} bytes, expected {want} for {w}x{h} rgba")]
    FrameSize {
        /// Buffer length that was passed in.
        got  : usize,
        /// Length implied by the dimensions.
        want : usize,
        /// Frame width in pixels.
        w    : u32,
        /// Frame height in pixels.
        h    : u32,
    },

    /// A previous `detect` call panicked while holding a session lock, so the session can
    /// no longer be trusted.
    #[error("inference session lock is poisoned")]
    Poisoned,

    /// onnxruntime reported an error while building or running a session.
    #[error(transparent)]
    Ort(#[from] ort::Error),
}
