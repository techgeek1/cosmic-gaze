//! Talking to the daemon over the session bus.
//!
//! The applet never holds a live subscription to the daemon: it asks for every property
//! at once ([`poll`]) on a timer, which survives the daemon restarting or not being there
//! at all, and calls the methods as fire-and-forget tasks. One bus connection is made
//! lazily and kept for the life of the process. Starting the daemon is the one thing
//! not done over the bus: [`start`] runs the installed `gazed`, and keeps the child so
//! that [`stop`] can signal a daemon that does not answer its bus `Quit`.

use std::fs::{self, OpenOptions};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

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

/// How long a daemon gets to exit after its bus `Quit` before it is signalled, and
/// again after `SIGTERM` before `SIGKILL`. The daemon's own stop grace is five seconds;
/// a daemon still there after that is not winding down.
const STOP_GRACE: Duration = Duration::from_secs(6);

/// How often [`stop`] checks whether the daemon has exited.
const STOP_POLL: Duration = Duration::from_millis(100);

/// The daemon this applet started, until it is seen to exit. Only this applet's own
/// child: a daemon started elsewhere is reachable over the bus alone.
static CHILD: Mutex<Option<Child>> = Mutex::new(None);

/// Runs the daemon. It reads its files from the XDG locations and logs to
/// [`LOG_NAME`]; whether it came up is what the next poll says. The child is kept for
/// [`stop`], and reaped by [`reap`] on every poll so a daemon that exits on its own
/// does not linger as a zombie of the panel.
pub fn start() -> Result<()> {
    reap();

    let state = Paths::xdg().state_dir;

    fs::create_dir_all(&state).with_context(|| format!("creating {}", state.display()))?;

    let log_path = state.join(LOG_NAME);
    let log      = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;

    let child = Command::new(DAEMON)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .with_context(|| format!("running {DAEMON}"))?;

    tracing::info!(pid = child.id(), log = %log_path.display(), "daemon started");

    *CHILD.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);

    Ok(())
}

/// Collects the daemon this applet started if it has exited, logging its status.
/// Called on every poll and before every start; harmless when there is nothing to reap.
pub fn reap() {
    let mut slot = CHILD.lock().unwrap_or_else(|e| e.into_inner());

    let exited = match slot.as_mut() {
        Some(child) => match child.try_wait() {
            Ok(Some(status)) => {
                tracing::info!(%status, "daemon exited");

                true
            }

            Ok(None) => false,

            Err(e) => {
                tracing::warn!("waiting for the daemon failed: {e}");

                true
            }
        },

        None => false,
    };

    if exited {
        *slot = None;
    }
}

/// Asks the daemon to end its session and exit, and sees that it does. The bus `Quit`
/// is the polite way and the only way to a daemon another applet started; for this
/// applet's own child, a daemon still running after [`STOP_GRACE`] gets `SIGTERM`, and
/// after another grace `SIGKILL`. The waiting happens on a thread of its own, so the
/// popup stays live; the next polls show the daemon going.
pub async fn stop() {
    quit().await;

    if CHILD.lock().unwrap_or_else(|e| e.into_inner()).is_none() {
        return;
    }

    if let Err(e) = thread::Builder::new()
        .name("gazed-stop".to_string())
        .spawn(ensure_stopped)
    {
        tracing::warn!("cannot watch the daemon stop: {e}");
    }
}

/// Asks the daemon over the bus to end its session and exit.
async fn quit() {
    let Some(proxy) = gaze_proxy().await else {
        return;
    };

    if let Err(e) = proxy.quit().await {
        tracing::warn!("daemon quit failed: {e}");
    }
}

/// Waits for this applet's daemon to exit, escalating to signals when it does not.
fn ensure_stopped() {
    for signal in [libc::SIGTERM, libc::SIGKILL] {
        let since = Instant::now();

        while since.elapsed() < STOP_GRACE {
            reap();

            if CHILD.lock().unwrap_or_else(|e| e.into_inner()).is_none() {
                return;
            }

            thread::sleep(STOP_POLL);
        }

        let slot = CHILD.lock().unwrap_or_else(|e| e.into_inner());

        let Some(child) = slot.as_ref() else {
            return;
        };

        tracing::warn!(pid = child.id(), signal = signal, "daemon did not exit after quit, signalling");

        // The pid is this applet's unreaped child, so it cannot have been reused.
        // SAFETY: `kill` has no memory-safety preconditions.
        unsafe {
            libc::kill(child.id() as libc::pid_t, signal);
        }
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
/// Also the reaper's tick.
pub async fn poll() -> Option<Status> {
    reap();

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

/// Tells the daemon whether the popup is open, so it holds off scrolling under it.
pub async fn set_popup_open(open: bool) {
    let Some(proxy) = gaze_proxy().await else {
        return;
    };

    if let Err(e) = proxy.set_popup_open(open).await {
        tracing::debug!(open, "daemon popup call failed: {e}");
    }
}

/// Asks the daemon for the quick calibration.
pub async fn calibrate() {
    let Some(proxy) = gaze_proxy().await else {
        return;
    };

    if let Err(e) = proxy.calibrate().await {
        tracing::warn!("daemon calibrate failed: {e}");
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
