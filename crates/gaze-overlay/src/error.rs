//! Failures the overlay can report.

use smithay_client_toolkit::error::GlobalError;
use smithay_client_toolkit::reexports::calloop::Error as CalloopError;
use smithay_client_toolkit::reexports::client::globals::{BindError, GlobalError as InitError};
use smithay_client_toolkit::reexports::client::{ConnectError, DispatchError};
use smithay_client_toolkit::shm::CreatePoolError;
use smithay_client_toolkit::shm::slot::CreateBufferError;
use thiserror::Error;

/// Everything that can go wrong setting up or running the overlay.
#[derive(Debug, Error)]
pub enum OverlayError {
    /// No compositor to talk to: `WAYLAND_DISPLAY` unset, or the socket refused us.
    #[error("cannot connect to the wayland compositor: {0}")]
    Connect(#[from] ConnectError),

    /// The initial registry handshake failed.
    #[error("wayland registry handshake failed: {0}")]
    Registry(#[from] InitError),

    /// The compositor is missing a global the overlay cannot work without, most likely
    /// `zwlr_layer_shell_v1`.
    #[error("compositor is missing a required protocol: {0}")]
    Bind(#[from] BindError),

    /// A bound global turned out to be unusable.
    #[error("wayland global is unusable: {0}")]
    Global(#[from] GlobalError),

    /// The connection broke while talking to the compositor.
    #[error("wayland communication failed: {0}")]
    Dispatch(#[from] DispatchError),

    /// The shared memory pool could not be created.
    #[error("cannot create the shm pool: {0}")]
    Pool(#[from] CreatePoolError),

    /// A buffer could not be carved out of the pool.
    #[error("cannot create an shm buffer: {0}")]
    Buffer(#[from] CreateBufferError),

    /// Growing the pool for a new surface failed.
    #[error("shm allocation failed: {0}")]
    Io(#[from] std::io::Error),

    /// The overlay thread could not be started.
    #[error("cannot start the overlay thread: {0}")]
    Spawn(std::io::Error),

    /// The calloop event loop could not be built or driven, or the overlay thread it was
    /// running on has gone away. Carries a message because calloop's insert errors are
    /// generic over the source being inserted and cannot be stored as-is.
    #[error("overlay event loop: {0}")]
    EventLoop(String),
}

// --- Conversions ---

impl From<CalloopError> for OverlayError {
    fn from(e: CalloopError) -> Self {
        OverlayError::EventLoop(e.to_string())
    }
}
