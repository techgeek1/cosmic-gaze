//! Talking to the daemon over the session bus.
//!
//! The applet never holds a live subscription to the daemon: it asks for every property
//! at once ([`poll`]) on a timer, which survives the daemon restarting or not being there
//! at all, and calls the methods as fire-and-forget tasks. One bus connection is made
//! lazily and kept for the life of the process. Starting the daemon is the one thing
//! not done over the bus: [`start`] runs the installed `gazed`.

use std::fs::{self, OpenOptions};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::thread;

use anyhow::{Context, Result};
use gaze_config::{BUS_NAME, BUS_PATH, GazeProxy, Paths, Status};
use tracing::debug;
use zbus::Connection;
use zbus::fdo::PropertiesProxy;
use zbus::names::InterfaceName;

/// The session bus, connected on first use.
static CONNECTION: OnceLock<Connection> = OnceLock::new();

/// The daemon's binary, found on the panel's `PATH` (`just install` puts it under
/// `~/.local/bin`).
const DAEMON: &str = "gazed";

/// The daemon's log, under the XDG state directory, appended to across runs.
const LOG_NAME: &str = "gazed.log";

/// Runs the daemon. It reads its files from the XDG locations and logs to
/// [`LOG_NAME`]; whether it came up is what the next poll says. A thread reaps it, so
/// a daemon that exits does not linger as a zombie of the panel.
pub fn start() -> Result<()> {
    let state = Paths::xdg().state_dir;

    fs::create_dir_all(&state).with_context(|| format!("creating {}", state.display()))?;

    let log_path = state.join(LOG_NAME);
    let log      = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;

    let mut child = Command::new(DAEMON)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .with_context(|| format!("running {DAEMON}"))?;

    tracing::info!(pid = child.id(), log = %log_path.display(), "daemon started");

    thread::Builder::new()
        .name("gazed-reaper".to_string())
        .spawn(move || match child.wait() {
            Ok(status) => tracing::info!(%status, "daemon exited"),
            Err(e)     => tracing::warn!("waiting for the daemon failed: {e}"),
        })
        .context("spawning the reaper thread")?;

    Ok(())
}

/// Asks the daemon to end its session and exit.
pub async fn quit() {
    let Some(proxy) = gaze_proxy().await else {
        return;
    };

    if let Err(e) = proxy.quit().await {
        tracing::warn!("daemon quit failed: {e}");
    }
}

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
