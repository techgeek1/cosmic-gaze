//! Talking to the daemon over the session bus.
//!
//! The applet never holds a live subscription to the daemon: it asks for every property
//! at once ([`poll`]) on a timer, which survives the daemon restarting or not being there
//! at all, and calls the three methods as fire-and-forget tasks. One bus connection is
//! made lazily and kept for the life of the process.

use std::sync::OnceLock;

use gaze_config::{BUS_NAME, BUS_PATH, GazeProxy, Status};
use tracing::debug;
use zbus::Connection;
use zbus::fdo::PropertiesProxy;
use zbus::names::InterfaceName;

/// The session bus, connected on first use.
static CONNECTION: OnceLock<Connection> = OnceLock::new();

/// The session bus connection, or `None` when there is no session bus, which is the
/// no-desktop case and not worth retrying every second.
async fn connection() -> Option<Connection> {
    if let Some(conn) = CONNECTION.get() {
        return Some(conn.clone());
    }

    match Connection::session().await {
        Ok(conn) => {
            let _ = CONNECTION.set(conn.clone());

            Some(conn)
        }

        Err(e) => {
            debug!("no session bus: {e}");

            None
        }
    }
}

/// Reads every property in one round trip. `None` when the daemon is not on the bus.
pub async fn poll() -> Option<Status> {
    let conn = connection().await?;

    let proxy = PropertiesProxy::builder(&conn)
        .destination(BUS_NAME).ok()?
        .path(BUS_PATH).ok()?
        .build()
        .await
        .ok()?;

    let interface = InterfaceName::try_from(BUS_NAME).ok()?;

    match proxy.get_all(interface).await {
        Ok(props) => Some(Status::from_properties(&props)),

        Err(e) => {
            debug!("daemon not answering: {e}");

            None
        }
    }
}

/// Pauses or resumes the daemon. Errors are logged, not returned: the next poll shows
/// whether it took.
pub async fn set_paused(paused: bool) {
    let Some(proxy) = gaze_proxy().await else {
        return;
    };

    let result = if paused { proxy.pause().await } else { proxy.resume().await };

    if let Err(e) = result {
        tracing::warn!(paused, "daemon pause call failed: {e}");
    }
}

/// Forgets the daemon's online offset.
pub async fn reset_offset() {
    let Some(proxy) = gaze_proxy().await else {
        return;
    };

    if let Err(e) = proxy.reset_offset().await {
        tracing::warn!("daemon reset offset failed: {e}");
    }
}

/// The daemon's proxy, or `None` with a log line when there is no bus.
async fn gaze_proxy() -> Option<GazeProxy<'static>> {
    let conn = connection().await?;

    match GazeProxy::new(&conn).await {
        Ok(proxy) => Some(proxy),

        Err(e) => {
            tracing::warn!("cannot reach the gaze daemon: {e}");

            None
        }
    }
}
