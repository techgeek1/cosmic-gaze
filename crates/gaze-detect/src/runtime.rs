//! onnxruntime setup: finding the system shared library and building CPU sessions.
//!
//! `ort` is built with `load-dynamic`, so no onnxruntime is linked at build time and none
//! is downloaded. The library is opened at first use from `ORT_DYLIB_PATH` if set, then
//! from the usual system locations. The GPU here is an RX 7900 XT with no CUDA, so the CPU
//! execution provider is the only one configured.

use std::path::Path;
use std::sync::Once;

use ort::execution_providers::CPUExecutionProvider;
use ort::session::Session;
use ort::session::builder::GraphOptimizationLevel;

use crate::error::{DetectError, Result};

/// Locations tried in order when `ORT_DYLIB_PATH` is not set. The first entry is where
/// Arch's `onnxruntime-cpu` package puts it.
const CANDIDATES : [&str; 3] = [
    "/usr/lib/libonnxruntime.so",
    "/usr/local/lib/libonnxruntime.so",
    "libonnxruntime.so",
];

/// Guards one-time initialisation of the ort environment.
static INIT : Once = Once::new();

// --- Runtime ---

/// Loads onnxruntime and commits the ort environment, at most once per process.
///
/// Safe to call from anywhere; later calls are no-ops. Failure to find the library is
/// reported by the first `Session` build rather than here, because `ort` only resolves the
/// dylib lazily when it needs the API table.
pub fn init() {
    INIT.call_once(|| {
        let explicit = std::env::var("ORT_DYLIB_PATH").ok().filter(|s| !s.is_empty());

        // An explicit override is left to ort's own lazy loader so the user sees ort's
        // error message for a bad path rather than a silent fallback to a system library.
        if explicit.is_some() {
            let _ = ort::init().with_name("gaze-detect").commit();

            return;
        }

        for path in CANDIDATES {
            if !Path::new(path).exists() && path.contains('/') {
                continue;
            }

            if let Ok(builder) = ort::init_from(path) {
                let _ = builder.with_name("gaze-detect").commit();

                return;
            }
        }

        let _ = ort::init().with_name("gaze-detect").commit();
    });
}

/// Opens `path` as a CPU inference session with full graph optimisation.
///
/// `intra_threads` sets onnxruntime's intra-op pool; zero leaves onnxruntime's own default,
/// which is one thread per physical core.
pub fn build_session(path: &Path, intra_threads: usize) -> Result<Session> {
    init();

    if !path.exists() {
        return Err(DetectError::MissingModel { path: path.to_path_buf() });
    }

    // Builder errors carry the builder itself (`ort::Error<SessionBuilder>`), so they need
    // flattening to the plain error before `?` can convert them.
    let mut builder = Session::builder()?
        .with_execution_providers([CPUExecutionProvider::default().build()])
        .map_err(ort::Error::from)?
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(ort::Error::from)?;

    if intra_threads > 0 {
        builder = builder.with_intra_threads(intra_threads).map_err(ort::Error::from)?;
    }

    Ok(builder.commit_from_file(path)?)
}
